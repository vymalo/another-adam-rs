//! `show { blocks, title? }` and `ui_catalog {}`: draw components on the person's screen, and read
//! which components there are.
//!
//! One `show` tool, not one tool per component: the set of components is the screen's and changes
//! with its version, the tool list of a model should not, and a screen is several blocks together.
//! The model reads the components with `ui_catalog` (their names, what they are for, and the JSON
//! Schema of each), then calls `show` with blocks `{component, ...properties}`; each is checked
//! against the catalog's schema, and what is wrong comes back to the model in words. A good call is
//! emitted as a run artifact `ui` of media type `application/a2ui+json` (the A2UI messages), which
//! the A2A server sends as a data part; the result the model reads is "Shown to the person.".

use std::sync::Arc;

use adam_a2a::A2UI_MEDIA_TYPE;
use adam_llm_agent::{Artifact, Tool, ToolCtx, ToolError, ToolOutput, parse_args};
use adam_model::ToolSpec;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::catalog::Catalog;
use crate::resolve::{Resolved, UiState};
use crate::surface::{safe_id, surface};

/// The name of the tool that draws.
pub const SHOW: &str = "show";

/// The name of the tool that lists the components.
pub const UI_CATALOG: &str = "ui_catalog";

/// The most blocks one `show` may draw.
pub const MAX_BLOCKS: usize = 16;

/// The component that asks the person questions. `show` refuses it: see [`CHOICES`] in the docs of
/// [`Show`].
pub(crate) const CHOICES: &str = "Choices";

/// The most bytes of the list of components in `show`'s description: a screen with sixty-four
/// components would put a page of text in front of the model at every turn.
const MAX_LIST_BYTES: usize = 2 * 1024;

/// The most characters of what a component is for, in that list: its first sentence, cut.
const MAX_ABOUT_CHARS: usize = 100;

/// The longest a message about a block may be, for the model.
const MAX_PROBLEM_BYTES: usize = 2 * 1024;

#[derive(Debug, Deserialize)]
struct Args {
    blocks: Vec<Value>,
    title: Option<String>,
}

/// Why there is nothing to draw with, as the text the model reads.
fn unavailable(resolved: Resolved) -> Result<Arc<Catalog>, ToolOutput> {
    match resolved {
        Resolved::Found(catalog) => Ok(catalog),
        Resolved::NoCatalog => Err(ToolOutput::error(
            "this screen has no component catalog; answer in text",
        )),
        Resolved::Unreadable(_) => Err(ToolOutput::error(
            "the screen's components could not be read; answer in text",
        )),
    }
}

fn cut(mut text: String) -> String {
    if text.len() > MAX_PROBLEM_BYTES {
        let mut end = MAX_PROBLEM_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(" ...");
    }
    text
}

/// The components of a `show`: the blocks as `b1`..`bn`, laid out in a `Column` (under a `Text` that
/// is the title, when there is one), the `Column` being the root. A single block with no title is the
/// root itself, so a screen with no `Column` can still show one component.
///
/// # Errors
///
/// What is wrong with the call, for the model.
fn components(
    catalog: &Catalog,
    blocks: &[Value],
    title: Option<&str>,
) -> Result<Vec<Value>, String> {
    if blocks.is_empty() || blocks.len() > MAX_BLOCKS {
        return Err(format!(
            "`blocks` has {} entries; it needs 1 to {MAX_BLOCKS}",
            blocks.len()
        ));
    }
    let mut made: Vec<Value> = Vec::with_capacity(blocks.len() + 2);
    let mut ids: Vec<String> = Vec::new();
    let title = title.map(str::trim).filter(|t| !t.is_empty());
    if let Some(title) = title {
        let text = json!({"id": "title", "component": "Text", "text": title, "variant": "h3"});
        catalog
            .validate(&text)
            .map_err(|problem| format!("the title cannot be shown: {problem}"))?;
        made.push(text);
        ids.push("title".to_owned());
    }
    for (n, block) in blocks.iter().enumerate() {
        let n = n + 1;
        let Value::Object(props) = block else {
            return Err(format!(
                "block {n} is not an object {{\"component\": ..., ...}}"
            ));
        };
        let Some(component) = props.get("component").and_then(Value::as_str) else {
            return Err(format!(
                "block {n} has no `component`; the components are: {}",
                catalog.names()
            ));
        };
        if component == CHOICES {
            return Err(format!(
                "block {n} is a `{CHOICES}` form, which `show` cannot draw: nobody could answer it, so it \
                 would be a dead form. To ask the person something, call `ask_user` with `question` and \
                 `choices`; the form is drawn and its answers come back as the result"
            ));
        }
        let mut instance: Map<String, Value> = props.clone();
        instance.insert("id".into(), Value::String(format!("b{n}")));
        let instance = Value::Object(instance);
        let name = instance["component"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        catalog
            .validate(&instance)
            .map_err(|problem| format!("block {n} ({name}): {problem}"))?;
        ids.push(format!("b{n}"));
        made.push(instance);
    }
    if made.len() == 1 {
        // One component and no title: it is the root.
        made[0]["id"] = Value::String("root".into());
        return Ok(made);
    }
    let column = json!({"id": "root", "component": "Column", "children": ids});
    catalog.validate(&column).map_err(|problem| {
        format!("this screen cannot lay several blocks out together ({problem}); show one block at a time")
    })?;
    made.insert(0, column);
    Ok(made)
}

/// What `show` is, before the screen says which components it has.
fn base_description() -> String {
    format!(
        "Show the person something on their screen, beside your text answer: a list of blocks, \
         each a component of the screen with its properties, drawn one under the other (at most \
         {MAX_BLOCKS}). Call `ui_catalog` first to learn the components and their properties; \
         a block that breaks a component's schema is refused with the reason, and you can \
         correct it. Use it when a card, a list or a diagram says it better than text; the \
         result is \"Shown to the person.\", and you still answer in text. \
         Never use it to ask the person something: a `{CHOICES}` form drawn here has nobody to answer \
         it, so `show` refuses it. Ask with `ask_user`, which takes `choices`."
    )
}

/// The description of `show` for a conversation whose screen has `catalog`: what it is, then each
/// component with the first sentence of what it is for, so that a block names a component that
/// exists on the first call (`ui_catalog` still gives the properties of each).
pub(crate) fn describe_show(catalog: &Catalog) -> String {
    let mut list = String::new();
    let mut left_out = 0usize;
    for component in catalog.components() {
        let about = if component.name() == CHOICES {
            // The catalog may say what the form is for; here only what `show` does with it.
            "a form of questions: not for `show`, ask with `ask_user` and `choices`".to_owned()
        } else {
            first_sentence(component.description())
        };
        let line = if about.is_empty() {
            format!("\n- {}", component.name())
        } else {
            format!("\n- {}: {about}", component.name())
        };
        if list.len() + line.len() > MAX_LIST_BYTES {
            left_out += 1;
        } else {
            list.push_str(&line);
        }
    }
    let more = if left_out > 0 {
        format!("\n- and {left_out} more: see `ui_catalog`")
    } else {
        String::new()
    };
    format!(
        "{} The components of this screen:{list}{more}",
        base_description()
    )
}

/// The first sentence of `text` (up to the first full stop followed by a space, or the first line
/// break), cut to [`MAX_ABOUT_CHARS`] characters.
fn first_sentence(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or_default();
    let end = line.find(". ").map_or(line.len(), |at| at + 1);
    let sentence = line[..end].trim();
    if sentence.chars().count() <= MAX_ABOUT_CHARS {
        return sentence.to_owned();
    }
    let mut cut: String = sentence.chars().take(MAX_ABOUT_CHARS - 1).collect();
    cut.push('…');
    cut
}

/// `show`. Made by [`Ui::tools`](crate::Ui::tools).
///
/// It draws what is to be **looked at**. A `Choices` block is refused: a form drawn by `show` is
/// not a question the conversation waits on, so it accepts no answer (the screen enables a form only
/// while the conversation is blocked on one), and the person would see a form that is dead. The
/// refusal tells the model to ask with [`AskUser`](crate::AskUser).
///
/// Its description is the one this crate makes ([`ThreadTools`](crate::ThreadTools) rewrites it each
/// turn, from the conversation's catalog, to list the screen's components); in a composition that
/// has no such source it tells the model to call `ui_catalog`.
#[derive(Clone)]
pub struct Show {
    state: Arc<UiState>,
}

impl std::fmt::Debug for Show {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Show").finish_non_exhaustive()
    }
}

impl Show {
    pub(crate) fn new(state: Arc<UiState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl Tool for Show {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: SHOW.to_owned(),
            description: base_description(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "blocks": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_BLOCKS,
                        "description": "The blocks, top to bottom",
                        "items": {
                            "type": "object",
                            "properties": {"component": {"type": "string", "description": "A component name from `ui_catalog`"}},
                            "required": ["component"],
                            "additionalProperties": true
                        }
                    },
                    "title": {"type": "string", "description": "A heading drawn above the blocks"}
                },
                "required": ["blocks"]
            }),
        }
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let args: Args = match parse_args(SHOW, args) {
            Ok(args) => args,
            Err(refusal) => return Ok(refusal),
        };
        let catalog = match unavailable(self.state.resolve(ctx.context_map()).await) {
            Ok(catalog) => catalog,
            Err(output) => return Ok(output),
        };
        let components = match components(&catalog, &args.blocks, args.title.as_deref()) {
            Ok(components) => components,
            Err(problem) => return Ok(ToolOutput::error(cut(problem))),
        };
        let id = format!("show-{}", safe_id(ctx.call_id()));
        let messages = surface(&id, catalog.catalog_id(), components);
        Ok(
            ToolOutput::text("Shown to the person.").with_artifact(Artifact::new(
                "ui",
                Some(A2UI_MEDIA_TYPE.to_owned()),
                messages,
            )),
        )
    }
}

/// `ui_catalog`. Made by [`Ui::tools`](crate::Ui::tools).
#[derive(Clone)]
pub struct UiCatalogTool {
    state: Arc<UiState>,
}

impl std::fmt::Debug for UiCatalogTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiCatalogTool").finish_non_exhaustive()
    }
}

impl UiCatalogTool {
    pub(crate) fn new(state: Arc<UiState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl Tool for UiCatalogTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: UI_CATALOG.to_owned(),
            description: "List the components the person's screen can draw: for each its name, what it \
                is for and the JSON Schema of its properties. Call it before `show`. The list is the \
                screen's, so it can differ from one conversation to the next."
                .to_owned(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(
            match unavailable(self.state.resolve(ctx.context_map()).await) {
                Ok(catalog) => ToolOutput::text(catalog.describe().to_string()),
                Err(output) => output,
            },
        )
    }
}
