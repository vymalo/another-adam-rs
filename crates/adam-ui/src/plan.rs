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
//! Each call also draws the plan on a screen that can, as **one** surface `plan-<12 hex digits of
//! a hash of the conversation's id>`: the same id on every call of the conversation (a task that is
//! continued updates the plan on the person's screen, it does not leave the old one stale), and the
//! same id for the artifact that carries it, which the run keeps once however often the plan
//! changes. What it draws is decided from the screen's catalog alone, never from a version number:
//!
//! 1. a catalog that validates a `Checklist` gets one `Checklist` as the root. A `dropped` item
//!    uses the state `dropped` if the catalog's `Checklist` allows it, and is otherwise left out of
//!    the list with the title saying so (`Fix the date parser (1 dropped)`). An item's note goes in
//!    the item's `note` if the catalog allows one, and otherwise after the label (` (note: ...)`).
//!    The label, the note and the title are each cut, with `…`, to the catalog's limit for them;
//! 2. else a catalog that validates a `Column` of `Text`s gets the lines of the result, packed into
//!    as many `Text`s as the `Column` holds children (several lines in one `Text` when there are
//!    more lines than that);
//! 3. else no surface: the result is the whole display. A call never fails for lack of a screen.
//!
//! **Scrub, then cut.** The surface is a pure function of `(plan, catalog)` ([`plan_surface`]) and
//! is built from a plan whose texts have already been scrubbed ([`ToolCtx::scrub`]), never the other
//! way round: a secret cut in half is not recognised by the scrubber that runs on the finished
//! result, and its first half would stay in a durable artifact.

use std::sync::Arc;

use adam_a2a::A2UI_MEDIA_TYPE;
use adam_llm_agent::{
    Artifact, MAX_PLAN_ITEMS, Plan, PlanItem, PlanStatus, StepIcon, StepStyle, Tool, ToolCtx,
    ToolError, ToolOutput, parse_args,
};
use adam_model::ToolSpec;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::catalog::Catalog;
use crate::resolve::{Resolved, UiState};
use crate::surface::surface;

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

/// How many children a `Column` is taken to hold when its schema does not say.
const DEFAULT_COLUMN_CHILDREN: usize = 50;

/// Said to a call with no plan to work on.
const NO_PLAN: &str = "There is no plan yet: start one with plan_work.";

/// Said to a subagent.
const SUBAGENT: &str = "A subagent keeps no plan: put what is left in your answer.";

/// What `plan_check` may set an item to.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckStatus {
    Pending,
    Doing,
    Done,
    Dropped,
}

impl CheckStatus {
    fn word(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Doing => "doing",
            Self::Done => "done",
            Self::Dropped => "dropped",
        }
    }

    fn stored(self) -> PlanStatus {
        match self {
            Self::Pending => PlanStatus::Pending,
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

/// The arguments of `plan_check`. Each is optional: with none, the plan comes back unchanged. An
/// item's number is read by [`item_number`], which knows the plan, so a number written the wrong
/// way is answered in the plan's words and not in a parser's.
#[derive(Debug, Deserialize)]
struct CheckArgs {
    id: Option<Value>,
    status: Option<CheckStatus>,
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AddArgs {
    items: Vec<String>,
    after: Option<Value>,
}

/// An item's number as a model writes it: `3`, or `"3"` (spaces and leading zeros allowed). Anything
/// else (`"+3"`, `"3.0"`, `-1`, `3.5`, text) is refused with the numbers there are.
fn item_number(value: &Value, items: usize) -> Result<u32, String> {
    let shown: String = match value {
        Value::String(text) => one_line(text).chars().take(40).collect(),
        other => other.to_string().chars().take(40).collect(),
    };
    let number = match value {
        Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
        Value::String(text) => {
            let text = text.trim();
            if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
                text.parse::<u32>().ok()
            } else {
                None
            }
        }
        _ => None,
    };
    number.ok_or_else(|| {
        format!("`{shown}` is not an item number: write a whole number from 1 to {items}.")
    })
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

/// The items of a call as clean lines: at least one, each 1 to [`MAX_ITEM_CHARS`] characters. The
/// most a plan holds is checked by the caller, which knows what the plan has already.
fn clean_items(items: &[String]) -> Result<Vec<String>, String> {
    if items.is_empty() {
        return Err(format!(
            "`items` needs 1 to {MAX_PLAN_ITEMS} entries; it has 0."
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

/// A note as a clean line, or `None` for one with no words (which clears a note).
fn clean_note(note: &str) -> Result<Option<String>, String> {
    let line = one_line(note);
    let chars = line.chars().count();
    if chars > MAX_NOTE_CHARS {
        return Err(format!(
            "The note is {chars} characters; at most {MAX_NOTE_CHARS}."
        ));
    }
    Ok((!line.is_empty()).then_some(line))
}

/// An error result with the plan after it, so the model sees the numbers it can use.
fn refusal(problem: String, plan: &Plan) -> ToolOutput {
    ToolOutput::error(format!("{problem}\n{}", plan.render()))
}

/// The error result for a call about an item that is not there.
fn no_such_item(id: u32, plan: &Plan) -> ToolOutput {
    refusal(
        format!(
            "There is no item {id}: the items are 1 to {}.",
            plan.items.len()
        ),
        plan,
    )
}

/// What every plan tool checks first: the call is the root run's, and not a subagent's.
#[allow(clippy::result_large_err)] // the refusal is a whole `ToolOutput`, as every tool returns it
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

/// `plan` with every text scrubbed by the layer around the tool ([`ToolCtx::scrub`]). It runs on
/// the whole plan on each call, so what an earlier call kept is scrubbed again with what the layer
/// knows now.
fn scrubbed(ctx: &ToolCtx, mut plan: Plan) -> Plan {
    plan.title = plan.title.take().map(|title| ctx.scrub(&title));
    for item in &mut plan.items {
        item.text = ctx.scrub(&item.text);
        item.note = item.note.take().map(|note| ctx.scrub(&note));
    }
    plan
}

/// The result of a good call: `first`, the plan as lines, the plan itself, and what the screen can
/// draw of it. Everything that is built from the plan's texts is built from the scrubbed plan.
async fn done(state: &UiState, ctx: &ToolCtx, first: String, plan: Plan) -> ToolOutput {
    let plan = scrubbed(ctx, plan);
    let content = format!("{first}\n{}", plan.render());
    let surface = match screen(state, ctx).await {
        Some(catalog) => plan_surface(&catalog, &plan, &plan_surface_id(ctx)),
        None => None,
    };
    let mut output = ToolOutput::text(content).with_plan(plan);
    if let Some(messages) = surface {
        output = output.with_artifact(
            Artifact::new("plan", Some(A2UI_MEDIA_TYPE.to_owned()), messages)
                .with_id(plan_surface_id(ctx)),
        );
    }
    output
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "item" } else { "items" }
}

// ---------------------------------------------------------------------------------------------
// The surface.

/// The id of the plan's surface, and of the artifact that carries it: `plan-` and 12 hex digits of
/// a hash of the conversation's id (the root run's id when the call is not in a conversation).
/// Stable for the conversation, so a task that is continued draws on the same surface. The orchestration
/// layer's UI catalog (version 8) reads an id that starts with `plan-` as a plan: drawn once,
/// updated in place, with no form, input or button (*unverified* against the published page,
/// 2026-11-14).
fn plan_surface_id(ctx: &ToolCtx) -> String {
    let root = ctx
        .conversation_id()
        .map_or_else(|| ctx.root_run_id().to_string(), str::to_owned);
    let digest = Sha256::digest(root.as_bytes());
    let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    format!("plan-{hex}")
}

/// The A2UI messages that draw `plan` on the screen of `catalog` as the surface `id`, or `None`
/// when it cannot. A pure function of the plan and the catalog: it cuts what it is given, so it is
/// to be given texts that are already scrubbed.
fn plan_surface(catalog: &Catalog, plan: &Plan, id: &str) -> Option<Value> {
    let components = checklist(catalog, plan).or_else(|| lines(catalog, plan))?;
    Some(surface(id, catalog.catalog_id(), components))
}

/// What a catalog's `Checklist` takes, found by asking it.
struct ChecklistKind {
    /// It has a `dropped` state.
    dropped: bool,
    /// An item has a `note`.
    note: bool,
    /// The most characters of an item's label.
    label_chars: usize,
    /// The most characters of the title.
    title_chars: usize,
    /// The most characters of an item's note.
    note_chars: usize,
}

fn probe(catalog: &Catalog, item: Value) -> bool {
    catalog
        .validate(&json!({"id": "probe", "component": "Checklist", "items": [item]}))
        .is_ok()
}

/// The `maxLength` the schema of `component` gives at `pointer`, or `default` (a limit under 2
/// leaves no room for text and its `…`, so it is not used).
fn limit(catalog: &Catalog, component: &str, pointer: &str, default: usize) -> usize {
    catalog
        .component(component)
        .and_then(|c| c.schema().pointer(pointer))
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n >= 2)
        .unwrap_or(default)
}

fn checklist_kind(catalog: &Catalog) -> Option<ChecklistKind> {
    catalog.component("Checklist")?;
    if !probe(catalog, json!({"label": "x", "state": "todo"})) {
        return None;
    }
    let item = "/properties/items/items/properties";
    Some(ChecklistKind {
        dropped: probe(catalog, json!({"label": "x", "state": "dropped"})),
        note: probe(catalog, json!({"label": "x", "state": "todo", "note": "x"})),
        label_chars: limit(
            catalog,
            "Checklist",
            &format!("{item}/label/maxLength"),
            MAX_ITEM_CHARS,
        ),
        title_chars: limit(
            catalog,
            "Checklist",
            "/properties/title/maxLength",
            usize::MAX,
        ),
        note_chars: limit(
            catalog,
            "Checklist",
            &format!("{item}/note/maxLength"),
            MAX_NOTE_CHARS,
        ),
    })
}

/// `text` of at most `max` characters (at least 2): cut at a character boundary and ended with `…`.
fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max - 1).collect();
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
                entry.insert("label".into(), json!(cut(&item.text, kind.label_chars)));
                entry.insert("note".into(), json!(cut(note, kind.note_chars)));
            }
            (Some(note), false) => {
                let label = format!("{} (note: {note})", item.text);
                entry.insert("label".into(), json!(cut(&label, kind.label_chars)));
            }
            (None, _) => {
                entry.insert("label".into(), json!(cut(&item.text, kind.label_chars)));
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
    let root = json!({
        "id": "root",
        "component": "Checklist",
        "title": cut(&title, kind.title_chars),
        "items": items
    });
    catalog.validate(&root).ok()?;
    Some(vec![root])
}

/// A `Column` of `Text`s holding the lines of the result, when the catalog validates them. The
/// first line is the heading; the others are packed, in order, into at most as many `Text`s as the
/// `Column` has room for, several lines (joined by a line break) to a `Text` when there are more
/// lines than that.
fn lines(catalog: &Catalog, plan: &Plan) -> Option<Vec<Value>> {
    let rendered = plan.render();
    let all: Vec<&str> = rendered.lines().collect();
    let (heading, rest) = all.split_first()?;
    let room = limit(
        catalog,
        "Column",
        "/properties/children/maxItems",
        DEFAULT_COLUMN_CHILDREN,
    )
    .saturating_sub(1)
    .max(1);
    let per_text = rest.len().div_ceil(room).max(1);
    let mut texts: Vec<(String, bool)> = vec![((*heading).to_owned(), true)];
    texts.extend(rest.chunks(per_text).map(|chunk| (chunk.join("\n"), false)));
    let mut made: Vec<Value> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    for (n, (text, heading)) in texts.into_iter().enumerate() {
        let id = format!("l{}", n + 1);
        let mut component = json!({"id": id, "component": "Text", "text": text});
        if heading {
            component["variant"] = json!("h3");
        }
        catalog.validate(&component).ok()?;
        ids.push(id);
        made.push(component);
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
                whole plan; plan_check with no arguments shows it again. Example: {\"title\":\"Fix the date \
                parser\",\"items\":[\"Reproduce the failing test\",\"Fix parse_date for time zones\",\"Run \
                the checks\",\"Open the pull request\"]}"
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
                `dropped` when it is no longer needed (say why in `note`), `pending` to put it back. `id` is \
                the number shown before the item. `note` replaces the item's note; an empty `note` clears it, \
                and leaving it out keeps it. With no arguments the plan comes back unchanged, to read it \
                again; do not call it again to re-read an unchanged plan. Returns the whole plan."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "minimum": 1, "description": "The number shown before the item"},
                    "status": {"enum": ["pending", "doing", "done", "dropped"]},
                    "note": {"type": "string", "maxLength": MAX_NOTE_CHARS, "description": "Why, or where the work is; empty clears the note"}
                },
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
        if args.id.is_none() && args.status.is_none() && args.note.is_none() {
            return Ok(done(&self.state, ctx, "Plan unchanged.".to_owned(), plan).await);
        }
        let Some(id) = &args.id else {
            return Ok(refusal(
                "Say which item: `id` is the number shown before it.".to_owned(),
                &plan,
            ));
        };
        let id = match item_number(id, plan.items.len()) {
            Ok(id) => id,
            Err(problem) => return Ok(refusal(problem, &plan)),
        };
        if args.status.is_none() && args.note.is_none() {
            return Ok(refusal(
                format!("Say what to set on item {id}: `status`, `note`, or both."),
                &plan,
            ));
        }
        let note = match args.note.as_deref().map(clean_note).transpose() {
            Ok(note) => note,
            Err(problem) => return Ok(ToolOutput::error(problem)),
        };
        let Some(item) = plan.items.iter_mut().find(|i| i.id == id) else {
            return Ok(no_such_item(id, &plan));
        };
        if let Some(status) = args.status {
            item.status = status.stored();
        }
        let first = match (args.status, note) {
            (Some(status), note) => {
                if let Some(note) = note {
                    item.note = note;
                }
                format!("Item {id}: {}.", status.word())
            }
            (None, Some(None)) => {
                item.note = None;
                format!("Item {id}: note cleared.")
            }
            (None, Some(note)) => {
                item.note = note;
                format!("Item {id}: note set.")
            }
            (None, None) => format!("Item {id}: unchanged."),
        };
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
        let at = match args.after.as_ref().filter(|v| !v.is_null()) {
            None => plan.items.len(),
            Some(after) => {
                let after = match item_number(after, plan.items.len()) {
                    Ok(after) => after,
                    Err(problem) => return Ok(refusal(problem, &plan)),
                };
                match plan.items.iter().position(|i| i.id == after) {
                    Some(position) => position + 1,
                    None => return Ok(no_such_item(after, &plan)),
                }
            }
        };
        // The numbers go on from the highest there is, not from how many items there are.
        let first_id = plan
            .items
            .iter()
            .map(|i| i.id)
            .max()
            .map_or(1, |highest| highest.saturating_add(1));
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
