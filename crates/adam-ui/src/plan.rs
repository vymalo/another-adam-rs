//! `plan_work`, `plan_check` and `plan_add`: the plan every root agent can keep (ADR 0044 decision 5).
//!
//! The plan is the conversation's state (`Conversation::plan` of `adam-llm-agent`): each tool reads the
//! plan its call starts from with `ToolCtx::plan`, and returns the **whole new plan** in its output,
//! which the loop applies from a result that is not an error. The result the model reads is built
//! only from that state (a first line, then [`Plan::render`]), so the same plan gives the same
//! bytes, the newest plan is always at the end of the history, and nothing of it is in the system
//! prompt (decision 1).
//!
//! # What the person sees
//!
//! Each call also draws the plan on a screen that can, as one surface `plan-<run id>` (the same id
//! on every call of a run, so each call replaces what the one before drew). What it draws is decided
//! from the screen's catalog alone, never from a version number:
//!
//! 1. a catalog that validates a `Checklist` gets one `Checklist` as the root. A `dropped` item
//!    uses the state `dropped` if the catalog's `Checklist` allows it, and is otherwise left out of
//!    the list with the title saying so (`Fix the date parser (1 dropped)`). An item's note goes in
//!    the item's `note` if the catalog allows one, and otherwise after the label (` (note: ...)`), the
//!    label being cut, with `…`, to the catalog's limit for it;
//! 2. else a catalog that validates a `Column` of `Text`s gets one `Text` per line of the result;
//! 3. else no surface: the result is the whole display. A call never fails for lack of a screen.

use std::sync::Arc;

use adam_a2a::A2UI_MEDIA_TYPE;
use adam_llm_agent::{
    Artifact, MAX_PLAN_ITEMS, Plan, PlanItem, PlanStatus, StepIcon, StepStyle, Tool, ToolCtx,
    ToolError, ToolOutput, parse_args,
};
use adam_model::ToolSpec;
use async_trait::async_trait;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

use crate::catalog::Catalog;
use crate::resolve::{Resolved, UiState};
use crate::surface::{safe_id, surface};

/// The name of the tool that writes the plan.
pub const PLAN_WORK: &str = "plan_work";

/// The name of the tool that sets one item of the plan.
pub const PLAN_CHECK: &str = "plan_check";

/// The name of the tool that adds items to the plan.
pub const PLAN_ADD: &str = "plan_add";

/// The longest an item's text may be, in characters.
const MAX_ITEM_CHARS: usize = 200;

/// The longest a title may be, in characters.
const MAX_TITLE_CHARS: usize = 80;

/// The longest a note may be, in characters.
const MAX_NOTE_CHARS: usize = 200;

/// Said to a call with no plan to work on.
const NO_PLAN: &str = "There is no plan yet: start one with plan_work.";

/// Said to a subagent.
const SUBAGENT: &str = "A subagent keeps no plan: put what is left in your answer.";

/// A number as a model writes it: `3`, or `"3"`.
fn number<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Number {
        Int(u32),
        Text(String),
    }
    match Number::deserialize(deserializer)? {
        Number::Int(n) => Ok(n),
        Number::Text(text) => text.trim().parse().map_err(|_| {
            serde::de::Error::custom(format!("`{text}` is not a number: write the item's number"))
        }),
    }
}

fn optional_number<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u32>, D::Error> {
    #[derive(Deserialize)]
    struct Wrapped(#[serde(deserialize_with = "number")] u32);
    Ok(Option::<Wrapped>::deserialize(deserializer)?.map(|w| w.0))
}

/// What `plan_check` may set an item to.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckStatus {
    Doing,
    Done,
    Dropped,
}

impl CheckStatus {
    fn word(self) -> &'static str {
        match self {
            Self::Doing => "doing",
            Self::Done => "done",
            Self::Dropped => "dropped",
        }
    }

    fn stored(self) -> PlanStatus {
        match self {
            Self::Doing => PlanStatus::Doing,
            Self::Done => PlanStatus::Done,
            Self::Dropped => PlanStatus::Dropped,
        }
    }
}

#[derive(Debug, Deserialize)]
struct WorkArgs {
    title: Option<String>,
    items: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CheckArgs {
    #[serde(deserialize_with = "number")]
    id: u32,
    status: CheckStatus,
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AddArgs {
    items: Vec<String>,
    #[serde(default, deserialize_with = "optional_number")]
    after: Option<u32>,
}

/// `text` as one line: every run of whitespace, line breaks, control and invisible format
/// characters is one space, and the ends are trimmed.
fn one_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut gap = false;
    for c in text.chars() {
        if c.is_whitespace() || c.is_control() || crate::is_format_or_control(c) {
            gap = true;
            continue;
        }
        if gap && !out.is_empty() {
            out.push(' ');
        }
        gap = false;
        out.push(c);
    }
    out
}

/// The items of a call as clean lines: 1 to [`MAX_PLAN_ITEMS`] of them, each 1 to
/// [`MAX_ITEM_CHARS`] characters.
fn clean_items(items: &[String]) -> Result<Vec<String>, String> {
    if items.is_empty() || items.len() > MAX_PLAN_ITEMS {
        return Err(format!(
            "`items` needs 1 to {MAX_PLAN_ITEMS} entries; it has {}.",
            items.len()
        ));
    }
    let mut clean = Vec::with_capacity(items.len());
    for (n, item) in items.iter().enumerate() {
        let line = one_line(item);
        let chars = line.chars().count();
        if chars == 0 {
            return Err(format!("Item {} is empty.", n + 1));
        }
        if chars > MAX_ITEM_CHARS {
            return Err(format!(
                "Item {} is {chars} characters; at most {MAX_ITEM_CHARS}.",
                n + 1
            ));
        }
        clean.push(line);
    }
    Ok(clean)
}

/// A note as a clean line, or `None` for one with no words.
fn clean_note(note: Option<&str>) -> Result<Option<String>, String> {
    let Some(note) = note else { return Ok(None) };
    let line = one_line(note);
    let chars = line.chars().count();
    if chars > MAX_NOTE_CHARS {
        return Err(format!(
            "The note is {chars} characters; at most {MAX_NOTE_CHARS}."
        ));
    }
    Ok((!line.is_empty()).then_some(line))
}

/// The error result with the plan after it, for a call about an item that is not there.
fn no_such_item(id: u32, plan: &Plan) -> ToolOutput {
    ToolOutput::error(format!(
        "There is no item {id}: the items are 1 to {}.\n{}",
        plan.items.len(),
        plan.render()
    ))
}

/// What every plan tool checks first: the call is the root run's, and not a subagent's.
fn root_only(ctx: &ToolCtx) -> Result<(), ToolOutput> {
    if ctx.run_id() == ctx.root_run_id() {
        Ok(())
    } else {
        Err(ToolOutput::error(SUBAGENT))
    }
}

/// The catalog the screen has now, if it has one that can be read.
async fn screen(state: &UiState, ctx: &ToolCtx) -> Option<Arc<Catalog>> {
    match state.resolve(ctx.context_map()).await {
        Resolved::Found(catalog) => Some(catalog),
        Resolved::NoCatalog | Resolved::Unreadable(_) => None,
    }
}

/// The result of a good call: `first`, the plan as lines, the plan itself, and what the screen can
/// draw of it.
async fn done(state: &UiState, ctx: &ToolCtx, first: String, plan: Plan) -> ToolOutput {
    let content = format!("{first}\n{}", plan.render());
    let surface = match screen(state, ctx).await {
        Some(catalog) => plan_surface(&catalog, &plan, ctx),
        None => None,
    };
    let mut output = ToolOutput::text(content).with_plan(plan);
    if let Some(messages) = surface {
        output = output.with_artifact(Artifact::new(
            "plan",
            Some(A2UI_MEDIA_TYPE.to_owned()),
            messages,
        ));
    }
    output
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "item" } else { "items" }
}

// ---------------------------------------------------------------------------------------------
// The surface.

/// The A2UI messages that draw `plan` on the screen of `catalog`, or `None` when it cannot.
fn plan_surface(catalog: &Catalog, plan: &Plan, ctx: &ToolCtx) -> Option<Value> {
    let id = format!("plan-{}", safe_id(&ctx.run_id().to_string()));
    let components = checklist(catalog, plan).or_else(|| lines(catalog, plan))?;
    Some(surface(&id, catalog.catalog_id(), components))
}

/// What a catalog's `Checklist` takes, found by asking it.
struct ChecklistKind {
    /// It has a `dropped` state.
    dropped: bool,
    /// An item has a `note`.
    note: bool,
    /// The most characters of an item's label.
    label_chars: usize,
}

fn probe(catalog: &Catalog, item: Value) -> bool {
    catalog
        .validate(&json!({"id": "probe", "component": "Checklist", "items": [item]}))
        .is_ok()
}

fn checklist_kind(catalog: &Catalog) -> Option<ChecklistKind> {
    let component = catalog.component("Checklist")?;
    if !probe(catalog, json!({"label": "x", "state": "todo"})) {
        return None;
    }
    let label_chars = component
        .schema()
        .pointer("/properties/items/items/properties/label/maxLength")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n >= 2)
        .unwrap_or(MAX_ITEM_CHARS);
    Some(ChecklistKind {
        dropped: probe(catalog, json!({"label": "x", "state": "dropped"})),
        note: probe(catalog, json!({"label": "x", "state": "todo", "note": "x"})),
        label_chars,
    })
}

/// `label` of at most `max` characters: cut at a character boundary and ended with `…`.
fn cut_label(label: String, max: usize) -> String {
    if label.chars().count() <= max {
        return label;
    }
    let mut cut: String = label.chars().take(max - 1).collect();
    cut.push('…');
    cut
}

/// One `Checklist`, as the root, when the catalog validates one with the plan's items.
fn checklist(catalog: &Catalog, plan: &Plan) -> Option<Vec<Value>> {
    let kind = checklist_kind(catalog)?;
    let mut items: Vec<Value> = Vec::with_capacity(plan.items.len());
    let mut left_out = 0usize;
    for item in &plan.items {
        let state = match item.status {
            PlanStatus::Pending => "todo",
            PlanStatus::Doing => "doing",
            PlanStatus::Done => "done",
            PlanStatus::Dropped if kind.dropped => "dropped",
            PlanStatus::Dropped => {
                left_out += 1;
                continue;
            }
        };
        let mut entry = serde_json::Map::new();
        match (&item.note, kind.note) {
            (Some(note), true) => {
                entry.insert(
                    "label".into(),
                    json!(cut_label(item.text.clone(), kind.label_chars)),
                );
                entry.insert("note".into(), json!(note));
            }
            (Some(note), false) => {
                let label = format!("{} (note: {note})", item.text);
                entry.insert("label".into(), json!(cut_label(label, kind.label_chars)));
            }
            (None, _) => {
                entry.insert(
                    "label".into(),
                    json!(cut_label(item.text.clone(), kind.label_chars)),
                );
            }
        }
        entry.insert("state".into(), json!(state));
        items.push(Value::Object(entry));
    }
    if items.is_empty() {
        return None;
    }
    let title = plan.title.as_deref().unwrap_or("Plan");
    let title = if left_out > 0 {
        format!("{title} ({left_out} dropped)")
    } else {
        title.to_owned()
    };
    let root = json!({"id": "root", "component": "Checklist", "title": title, "items": items});
    catalog.validate(&root).ok()?;
    Some(vec![root])
}

/// A `Column` of `Text`s, one per line of the result, when the catalog validates them.
fn lines(catalog: &Catalog, plan: &Plan) -> Option<Vec<Value>> {
    let rendered = plan.render();
    let mut made: Vec<Value> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    for (n, line) in rendered.lines().enumerate() {
        let id = format!("l{}", n + 1);
        let mut text = json!({"id": id, "component": "Text", "text": line});
        if n == 0 {
            text["variant"] = json!("h3");
        }
        catalog.validate(&text).ok()?;
        ids.push(id);
        made.push(text);
    }
    let column = json!({"id": "root", "component": "Column", "children": ids});
    catalog.validate(&column).ok()?;
    made.insert(0, column);
    Some(made)
}

// ---------------------------------------------------------------------------------------------
// The tools.

fn items_schema(description: &str) -> Value {
    json!({
        "type": "array",
        "minItems": 1,
        "maxItems": MAX_PLAN_ITEMS,
        "description": description,
        "items": {"type": "string", "minLength": 1, "maxLength": MAX_ITEM_CHARS}
    })
}

/// How the step of a plan call is drawn: the words of what it does, with the thinking icon. Each
/// tool has a title of its own (a person reading the steps sees "Update the plan", not three
/// steps called "Plan"; and a client that lists the titles finds them unique).
fn step(label: &str) -> StepStyle {
    StepStyle::default()
        .with_label(label)
        .with_icon(StepIcon::Think)
}

macro_rules! plan_tool {
    ($name:ident) => {
        #[derive(Clone)]
        #[doc = concat!("`", stringify!($name), "`. Made by [`Ui::tools`](crate::Ui::tools).")]
        pub struct $name {
            state: Arc<UiState>,
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct(stringify!($name)).finish_non_exhaustive()
            }
        }

        impl $name {
            pub(crate) fn new(state: Arc<UiState>) -> Self {
                Self { state }
            }
        }
    };
}

plan_tool!(PlanWork);
plan_tool!(PlanCheck);
plan_tool!(PlanAdd);

#[async_trait]
impl Tool for PlanWork {
    fn step_style(&self) -> StepStyle {
        step("Write the plan")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: PLAN_WORK.to_owned(),
            description: "Write the plan for this task as a short checklist the person sees, replacing any \
                earlier plan. Use it when the work has three or more steps or the person asked for several \
                things; not for a question or a one-step change. One action per item, in order, at most 50, \
                each under 200 characters. Then mark progress with plan_check (one item `doing` at a time, \
                `done` as soon as it is) and add what you discover with plan_add. Every call returns the \
                whole plan. Example: {\"title\":\"Fix the date parser\",\"items\":[\"Reproduce the failing \
                test\",\"Fix parse_date for time zones\",\"Run the checks\",\"Open the pull request\"]}"
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "maxLength": MAX_TITLE_CHARS, "description": "A short name for the plan"},
                    "items": items_schema("The items, in order, one action each")
                },
                "required": ["items"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        if let Err(refusal) = root_only(ctx) {
            return Ok(refusal);
        }
        let args: WorkArgs = match parse_args(PLAN_WORK, args) {
            Ok(args) => args,
            Err(refusal) => return Ok(refusal),
        };
        let title = args
            .title
            .as_deref()
            .map(one_line)
            .filter(|t| !t.is_empty());
        if let Some(title) = &title {
            let chars = title.chars().count();
            if chars > MAX_TITLE_CHARS {
                return Ok(ToolOutput::error(format!(
                    "The title is {chars} characters; at most {MAX_TITLE_CHARS}."
                )));
            }
        }
        if args.items.len() > MAX_PLAN_ITEMS {
            return Ok(ToolOutput::error(format!(
                "The plan would have {} items; at most {MAX_PLAN_ITEMS}. Merge items and call plan_work again.",
                args.items.len()
            )));
        }
        let texts = match clean_items(&args.items) {
            Ok(texts) => texts,
            Err(problem) => return Ok(ToolOutput::error(problem)),
        };
        let plan = Plan {
            title,
            items: texts
                .into_iter()
                .zip(1u32..)
                .map(|(text, id)| PlanItem {
                    id,
                    text,
                    status: PlanStatus::Pending,
                    note: None,
                })
                .collect(),
        };
        let first = format!(
            "Plan {} ({} {}).",
            if ctx.plan().is_some() {
                "replaced"
            } else {
                "saved"
            },
            plan.items.len(),
            plural(plan.items.len())
        );
        Ok(done(&self.state, ctx, first, plan).await)
    }
}

#[async_trait]
impl Tool for PlanCheck {
    fn step_style(&self) -> StepStyle {
        step("Update the plan")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: PLAN_CHECK.to_owned(),
            description: "Set one item of the plan: `doing` when you start it, `done` when it is finished, \
                `dropped` when it is no longer needed (say why in `note`). `id` is the number shown before \
                the item. Returns the whole plan."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "minimum": 1, "description": "The number shown before the item"},
                    "status": {"enum": ["doing", "done", "dropped"]},
                    "note": {"type": "string", "maxLength": MAX_NOTE_CHARS, "description": "Why, or where the work is"}
                },
                "required": ["id", "status"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        if let Err(refusal) = root_only(ctx) {
            return Ok(refusal);
        }
        let args: CheckArgs = match parse_args(PLAN_CHECK, args) {
            Ok(args) => args,
            Err(refusal) => return Ok(refusal),
        };
        let Some(mut plan) = ctx.plan().cloned() else {
            return Ok(ToolOutput::error(NO_PLAN));
        };
        let note = match clean_note(args.note.as_deref()) {
            Ok(note) => note,
            Err(problem) => return Ok(ToolOutput::error(problem)),
        };
        let Some(item) = plan.items.iter_mut().find(|i| i.id == args.id) else {
            return Ok(no_such_item(args.id, &plan));
        };
        item.status = args.status.stored();
        if note.is_some() {
            item.note = note;
        }
        let first = format!("Item {}: {}.", args.id, args.status.word());
        Ok(done(&self.state, ctx, first, plan).await)
    }
}

#[async_trait]
impl Tool for PlanAdd {
    fn step_style(&self) -> StepStyle {
        step("Add to the plan")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: PLAN_ADD.to_owned(),
            description:
                "Add items you found on the way, at the end or after item `after`; they get the \
                next numbers. Returns the whole plan."
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "items": items_schema("The items to add, in order, one action each"),
                    "after": {"type": "integer", "minimum": 1, "description": "Show the new items after this item; at the end when left out"}
                },
                "required": ["items"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        if let Err(refusal) = root_only(ctx) {
            return Ok(refusal);
        }
        let args: AddArgs = match parse_args(PLAN_ADD, args) {
            Ok(args) => args,
            Err(refusal) => return Ok(refusal),
        };
        let Some(mut plan) = ctx.plan().cloned() else {
            return Ok(ToolOutput::error(NO_PLAN));
        };
        let total = plan.items.len() + args.items.len();
        if total > MAX_PLAN_ITEMS {
            return Ok(ToolOutput::error(format!(
                "The plan would have {total} items; at most {MAX_PLAN_ITEMS}. Drop or merge items, or start again with plan_work."
            )));
        }
        let texts = match clean_items(&args.items) {
            Ok(texts) => texts,
            Err(problem) => return Ok(ToolOutput::error(problem)),
        };
        let at = match args.after {
            None => plan.items.len(),
            Some(after) => match plan.items.iter().position(|i| i.id == after) {
                Some(position) => position + 1,
                None => return Ok(no_such_item(after, &plan)),
            },
        };
        let first_id = u32::try_from(plan.items.len() + 1).unwrap_or(u32::MAX);
        let added: Vec<PlanItem> = texts
            .into_iter()
            .zip(first_id..)
            .map(|(text, id)| PlanItem {
                id,
                text,
                status: PlanStatus::Pending,
                note: None,
            })
            .collect();
        let numbers: Vec<String> = added.iter().map(|i| i.id.to_string()).collect();
        plan.items.splice(at..at, added);
        let first = format!(
            "Added {} {}: {}.",
            numbers.len(),
            plural(numbers.len()),
            numbers.join(", ")
        );
        Ok(done(&self.state, ctx, first, plan).await)
    }
}
