//! [`vymalo_inbound`]: how an A2A message becomes the agent's input when the sender is a screen
//! (the orchestration layer's chat): the person's A2UI actions read as answers, and what the
//! message says about the screen reaches the run as its inbound context.

use std::collections::HashMap;

use a2a::{Message, Part, PartContent};
use adam_a2a::{A2UI_MEDIA_TYPE, THREAD_TOOLS_EXTENSION, UI_CATALOG_EXTENSION};
use adam_runtime::Inbound;
use serde_json::{Map, Number, Value, json};

/// The context key under which a message that carries `ui-catalog/v1` says which catalog is
/// current: `{catalogId, version, digest}`.
pub const CONTEXT_UI_REF: &str = "vymalo.ui.ref";

/// The context key under which a message that carries the catalog inline holds it:
/// `{catalogId, version, digest, catalog}`. Only set when the inline catalog has the `catalogId`
/// of the ref.
pub const CONTEXT_UI_CATALOG: &str = "vymalo.ui.catalog";

/// The context key of the thread-tools grant: `{url, token, expiresAt}`. A credential; the entry
/// expires at `expiresAt`.
pub const CONTEXT_THREAD_TOOLS: &str = "vymalo.threadTools";

/// The most characters of an action's `context` that the text for the model carries.
pub const MAX_ACTION_CONTEXT_CHARS: usize = 4096;

/// The largest whole number a double holds exactly (2^53 - 1): what a catalog may contain.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// The inbound function of an agent that serves a screen: pass it to
/// [`RuntimeTaskBackend::with_inbound`](crate::RuntimeTaskBackend::with_inbound).
///
/// The default reading ([`default_inbound`](crate::default_inbound)) joins the text parts and
/// prints the data parts as JSON; that is right for a message from another agent. A message
/// from a screen carries three more things, all optional extensions the sender detects on the
/// agent's card (`docs/api/ui-catalog-v1.md`, `docs/api/thread-tools-v1.md` of
/// `vymalo/another-agentic-system`):
///
/// ```mermaid
/// sequenceDiagram
///     participant S as Sender (the screen's orchestrator)
///     participant B as RuntimeTaskBackend (inbound = vymalo_inbound)
///     participant R as Run (Conversation::context)
///     participant T as Tool (ToolCtx::context)
///     S->>B: message: text, or an A2UI action part, and metadata
///     Note over B: text = the text parts, an action rendered as what the person did
///     Note over B: context = ui-catalog/v1, an inline catalog, thread-tools/v1
///     B->>R: Inbound {text, context}, merged into the conversation
///     T->>R: ctx.context("vymalo.ui.ref") ...
/// ```
///
/// | Message part or metadata | Becomes |
/// |---|---|
/// | text parts | the text, joined by blank lines |
/// | an A2UI data part (`application/a2ui+json`) with an `action` that carries Choices answers | the text `The person answered through the interface:` and one line `- <question id>: <values>` per answer, an "other" as `other: "<text>"` |
/// | any other A2UI action | `The person used the interface: action "<name>" on surface "<id>" with context <compact JSON, cut at 4 KiB>` |
/// | any other data part | its JSON, as the default reading does |
/// | `metadata[ui-catalog/v1]` `{catalogId, version, digest, inline}` | context [`CONTEXT_UI_REF`] `{catalogId, version, digest}` |
/// | the renderer's capabilities (`a2uiClientCapabilities.v0.9.1`, else `.v0.9`, else `a2uiRendererCapabilities.v1.0`) holding an inline catalog with the ref's `catalogId` | context [`CONTEXT_UI_CATALOG`] `{catalogId, version, digest, catalog}` |
/// | `metadata[thread-tools/v1]` `{url, token, expiresAt}` | context [`CONTEXT_THREAD_TOOLS`], the same three members |
///
/// A message with none of the metadata has no `context` at all, so it changes nothing in the run.
/// The capability key is read under both `v0.9` and `v0.9.1` because A2UI's own files disagree on
/// it (*verified 2026-10-01*, `github.com/google/A2UI` `main`: the JSON schema keys by `v0.9`, the
/// extension page's example by `v0.9.1`).
///
/// **Numbers.** An A2A server holds the numbers of a message's metadata as doubles (they are a
/// protobuf `Struct`), so an inline catalog reads `maxLength: 256.0` and a `version` reads `2.0`.
/// The values that go into the context have every whole number written as an integer
/// ([`integral_numbers`]), which is what the digest of a catalog is taken over.
///
/// The thread-tools `token` is a credential: it is only put in the context (the durable state of
/// the run, until its `expiresAt`, see `Conversation::drop_expired_context`), never in a log
/// line.
///
/// # Errors
///
/// A message with no text (and no A2UI action to read as text) is rejected, as by
/// [`default_inbound`](crate::default_inbound).
pub fn vymalo_inbound(message: &Message) -> Result<Inbound, String> {
    let mut chunks: Vec<String> = Vec::new();
    for part in &message.parts {
        match &part.content {
            PartContent::Text(text) => chunks.push(text.clone()),
            PartContent::Data(data) if claims_a2ui(part) => {
                chunks.extend(action_texts(data));
            }
            PartContent::Data(data) => chunks.push(data.to_string()),
            _ => {}
        }
    }
    let text = chunks.join("\n\n");
    if text.trim().is_empty() {
        return Err("the message has no text".to_owned());
    }
    let mut payload = json!({ "text": text });
    if let Some(context) = message.metadata.as_ref().and_then(context_of)
        && !context.is_empty()
    {
        payload["context"] = Value::Object(context);
    }
    Ok(Inbound::new("message", payload).with_id(message.message_id.clone()))
}

/// Whether `part` says it carries A2UI: the A2A 1.0 `mediaType`, or the extension's
/// `metadata.mimeType`, is `application/a2ui+json` (parameters and case ignored).
pub(crate) fn claims_a2ui(part: &Part) -> bool {
    let says = |media: &str| {
        media
            .split(';')
            .next()
            .is_some_and(|m| m.trim().eq_ignore_ascii_case(A2UI_MEDIA_TYPE))
    };
    part.media_type.as_deref().is_some_and(says)
        || part
            .metadata
            .as_ref()
            .and_then(|m| m.get("mimeType"))
            .and_then(Value::as_str)
            .is_some_and(says)
}

// ---------------------------------------------------------------------------------------------
// Actions as text
// ---------------------------------------------------------------------------------------------

/// The text of every client action in an A2UI data part (an array of messages, or one).
fn action_texts(data: &Value) -> Vec<String> {
    let messages: Vec<&Value> = match data {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    messages
        .into_iter()
        .filter_map(|m| m.get("action"))
        .map(action_text)
        .collect()
}

/// One action as what the model reads.
fn action_text(action: &Value) -> String {
    if let Some(answers) = action.pointer("/context/answers").and_then(answers_text) {
        return answers;
    }
    let shown = |key: &str| {
        action
            .get(key)
            .and_then(Value::as_str)
            .map_or_else(|| "?".to_owned(), quoted)
    };
    let context = action.get("context").cloned().unwrap_or(Value::Null);
    format!(
        "The person used the interface: action {} on surface {} with context {}",
        shown("name"),
        shown("surfaceId"),
        cut(&context.to_string(), MAX_ACTION_CONTEXT_CHARS)
    )
}

/// The answers of a Choices (`[{id, values, other?}]`, in question order) as lines; `None` when
/// the value is not that shape, so that any other action keeps the generic reading.
fn answers_text(answers: &Value) -> Option<String> {
    let answers = answers.as_array().filter(|a| !a.is_empty())?;
    let mut lines = vec!["The person answered through the interface:".to_owned()];
    for answer in answers {
        let id = answer.get("id")?.as_str()?;
        let values = answer.get("values")?.as_array()?;
        let other = match answer.get("other") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => Some(text.as_str()),
            Some(_) => return None,
        };
        let mut chosen: Vec<String> = Vec::new();
        for value in values {
            chosen.push(plain_or_quoted(value.as_str()?));
        }
        if let Some(other) = other {
            chosen.push(format!("other: {}", quoted(other)));
        }
        lines.push(format!(
            "- {}: {}",
            plain_or_quoted(id),
            if chosen.is_empty() {
                "(nothing chosen)".to_owned()
            } else {
                chosen.join(", ")
            }
        ));
    }
    Some(lines.join("\n"))
}

/// `text` as it is when it is a plain token (what a Choices id or option value is), else as a JSON
/// string: what the person typed can hold newlines or look like another line of the answer.
fn plain_or_quoted(text: &str) -> String {
    let plain = !text.is_empty()
        && text.len() <= 64
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'));
    if plain { text.to_owned() } else { quoted(text) }
}

/// `text` as a JSON string.
fn quoted(text: &str) -> String {
    Value::String(text.to_owned()).to_string()
}

/// `text` cut to at most `max` characters, with a note when it was.
fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{} ... (cut: the context is longer)", &text[..at]),
        None => text.to_owned(),
    }
}

// ---------------------------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------------------------

/// What the message's metadata says about the screen, as the run's inbound context; `None` when
/// it says nothing this module reads.
fn context_of(metadata: &HashMap<String, Value>) -> Option<Map<String, Value>> {
    let mut context = Map::new();
    if let Some(reference) = metadata.get(UI_CATALOG_EXTENSION).and_then(catalog_ref) {
        if let Some(catalog) = inline_catalog(metadata, &reference) {
            context.insert(
                CONTEXT_UI_CATALOG.to_owned(),
                json!({
                    "catalogId": reference["catalogId"],
                    "version": reference["version"],
                    "digest": reference["digest"],
                    "catalog": catalog,
                }),
            );
        }
        context.insert(CONTEXT_UI_REF.to_owned(), reference);
    }
    if let Some(grant) = metadata.get(THREAD_TOOLS_EXTENSION).and_then(thread_tools) {
        context.insert(CONTEXT_THREAD_TOOLS.to_owned(), grant);
    }
    (!context.is_empty()).then_some(context)
}

/// `{catalogId, version, digest}` of the `ui-catalog/v1` metadata, with `version` as an integer;
/// `None` when a member is missing or has the wrong type.
fn catalog_ref(value: &Value) -> Option<Value> {
    let catalog_id = value.get("catalogId")?.as_str().filter(|s| !s.is_empty())?;
    let digest = value.get("digest")?.as_str().filter(|s| !s.is_empty())?;
    let version = whole_number(value.get("version")?)?;
    if version < 1 {
        return None;
    }
    Some(json!({"catalogId": catalog_id, "version": version, "digest": digest}))
}

/// The inline catalog the message carries for `reference`'s `catalogId`, if any, with its whole
/// numbers as integers.
fn inline_catalog(metadata: &HashMap<String, Value>, reference: &Value) -> Option<Value> {
    let wanted = reference["catalogId"].as_str()?;
    [
        ("a2uiClientCapabilities", "v0.9.1"),
        ("a2uiClientCapabilities", "v0.9"),
        ("a2uiRendererCapabilities", "v1.0"),
    ]
    .into_iter()
    .filter_map(|(key, version)| {
        metadata
            .get(key)?
            .get(version)?
            .get("inlineCatalogs")?
            .as_array()
    })
    .flatten()
    .find(|catalog| catalog.get("catalogId").and_then(Value::as_str) == Some(wanted))
    .map(|catalog| {
        let mut catalog = catalog.clone();
        integral_numbers(&mut catalog);
        catalog
    })
}

/// `{url, token, expiresAt}` of the `thread-tools/v1` metadata; `None` when one is missing or is
/// not a non-empty string. The value is copied exactly, so what a client later sends back as the
/// bearer token is the token it was given.
fn thread_tools(value: &Value) -> Option<Value> {
    let member = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    Some(json!({
        "url": member("url")?,
        "token": member("token")?,
        "expiresAt": member("expiresAt")?,
    }))
}

/// `number` as an integer when it is a whole number a double holds exactly (`2` and `2.0`).
fn whole_number(value: &Value) -> Option<i64> {
    let number = value.as_number()?;
    number.as_i64().or_else(|| {
        let f = number.as_f64()?;
        (f.fract() == 0.0 && f.abs() <= MAX_SAFE_INTEGER).then_some(f as i64)
    })
}

/// Write every whole number in `value` as an integer: `256.0` becomes `256`, as RFC 8785 writes
/// it and as the digest of a catalog is taken over. A2A hands a message's metadata over as doubles,
/// so a catalog that arrives in it reads `256.0` where the sender wrote `256`. Numbers that are not
/// whole, or beyond 2^53 - 1, are left as they are.
pub fn integral_numbers(value: &mut Value) {
    match value {
        Value::Number(number) => {
            if number.is_f64()
                && let Some(whole) = whole_number(&Value::Number(number.clone()))
            {
                *number = Number::from(whole);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(integral_numbers),
        Value::Object(map) => map.values_mut().for_each(integral_numbers),
        Value::Null | Value::Bool(_) | Value::String(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use a2a::Role;

    use super::*;

    fn text_message(text: &str) -> Message {
        Message::new(Role::User, vec![Part::text(text)])
    }

    fn a2ui_part(data: Value) -> Part {
        let mut part = Part::data(data).with_media_type(A2UI_MEDIA_TYPE);
        part.metadata = Some(HashMap::from([(
            "mimeType".to_owned(),
            json!(A2UI_MEDIA_TYPE),
        )]));
        part
    }

    fn action_message(action: Value) -> Message {
        Message::new(
            Role::User,
            vec![a2ui_part(json!([{"version": "v0.9.1", "action": action}]))],
        )
    }

    fn with_metadata(mut message: Message, metadata: Value) -> Message {
        message.metadata = Some(
            metadata
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        );
        message
    }

    const CATALOG_ID: &str = "https://agents.vymalo.com/a2ui/catalogs/chat";
    const DIGEST: &str = "sha256:4ed91bcc9db52d5e2262aef2091d2b3eeccbf5bfe51519d7326fdc6641fb7856";

    fn catalog() -> Value {
        // As an A2A server hands it over: every number a double.
        json!({"catalogId": CATALOG_ID, "components": {"Text": {
            "type": "object",
            "properties": {"text": {"type": "string", "maxLength": 4000.0, "minLength": 1.0}},
            "required": ["text"]}}})
    }

    // ---- the text ----

    #[test]
    fn plain_text_is_read_as_the_default_reading_does() {
        let m = text_message("add hello.txt");
        let ours = vymalo_inbound(&m).unwrap();
        let default = crate::default_inbound(&m).unwrap();
        assert_eq!(ours.payload, default.payload);
        assert_eq!(ours.payload, json!({"text": "add hello.txt"}));
        assert_eq!(ours.id, m.message_id);
        assert_eq!(ours.kind, "message");
        assert!(vymalo_inbound(&text_message("  ")).is_err());
        assert!(vymalo_inbound(&Message::new(Role::User, vec![Part::raw(vec![1])])).is_err());
    }

    #[test]
    fn a_data_part_that_is_not_a2ui_is_json_text_and_text_parts_are_joined() {
        let m = Message::new(
            Role::User,
            vec![
                Part::text("see"),
                Part::data(json!({"a": 1})),
                Part::text("this"),
            ],
        );
        assert_eq!(
            vymalo_inbound(&m).unwrap().payload,
            json!({"text": "see\n\n{\"a\":1}\n\nthis"})
        );
    }

    // ---- actions ----

    #[test]
    fn choices_answers_read_as_one_line_per_question() {
        let m = action_message(json!({
            "name": "answer", "surfaceId": "ask-1", "sourceComponentId": "root",
            "timestamp": "2026-10-01T10:00:00Z",
            "context": {"answers": [
                {"id": "db", "values": ["pg"]},
                {"id": "auth", "values": [], "other": "Keycloak"},
                {"id": "deploy", "values": ["k8s", "compose"], "other": "my \"own\"\nline"},
                {"id": "skipped", "values": []}
            ]}
        }));
        let got = vymalo_inbound(&m).unwrap();
        assert_eq!(
            got.payload["text"],
            "The person answered through the interface:\n\
             - db: pg\n\
             - auth: other: \"Keycloak\"\n\
             - deploy: k8s, compose, other: \"my \\\"own\\\"\\nline\"\n\
             - skipped: (nothing chosen)"
        );
        assert!(got.payload.get("context").is_none());
    }

    #[test]
    fn an_answer_with_a_value_that_is_not_a_plain_token_is_quoted() {
        let m = action_message(json!({
            "name": "answer", "surfaceId": "s", "sourceComponentId": "c", "timestamp": "t",
            "context": {"answers": [{"id": "a\nb", "values": ["x y", "ok-1"]}]}
        }));
        assert_eq!(
            vymalo_inbound(&m).unwrap().payload["text"],
            "The person answered through the interface:\n- \"a\\nb\": \"x y\", ok-1"
        );
    }

    #[test]
    fn answers_that_are_not_the_choices_shape_are_the_generic_reading() {
        for context in [
            json!({"answers": "no"}),
            json!({"answers": []}),
            json!({"answers": [{"id": "a"}]}),
            json!({"answers": [{"id": "a", "values": [1]}]}),
            json!({"answers": [{"id": "a", "values": [], "other": 3}]}),
            json!({"choice": "a"}),
        ] {
            let m = action_message(json!({
                "name": "go", "surfaceId": "s1", "sourceComponentId": "c", "timestamp": "t",
                "context": context
            }));
            let text = vymalo_inbound(&m).unwrap().payload["text"]
                .as_str()
                .unwrap()
                .to_owned();
            assert_eq!(
                text,
                format!(
                    "The person used the interface: action \"go\" on surface \"s1\" with context {context}"
                )
            );
        }
    }

    #[test]
    fn an_oversized_action_context_is_cut_and_says_so() {
        let m = action_message(json!({
            "name": "go", "surfaceId": "s", "sourceComponentId": "c", "timestamp": "t",
            "context": {"blob": "é".repeat(10_000)}
        }));
        let text = vymalo_inbound(&m).unwrap().payload["text"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            text.ends_with("... (cut: the context is longer)"),
            "{}",
            &text[text.len() - 60..]
        );
        assert!(text.chars().count() < MAX_ACTION_CONTEXT_CHARS + 200);
        assert!(text.contains("action \"go\""));
    }

    #[test]
    fn a_message_of_one_action_part_is_accepted_and_both_media_spellings_count() {
        let action = json!([{"version": "v0.9.1", "action": {
            "name": "go", "surfaceId": "s", "sourceComponentId": "c", "timestamp": "t",
            "context": {}}}]);
        // mediaType only, metadata.mimeType only, and a parameterised media type.
        let by_media =
            Part::data(action.clone()).with_media_type("Application/A2UI+JSON; charset=utf-8");
        let mut by_meta = Part::data(action.clone());
        by_meta.metadata = Some(HashMap::from([("mimeType".into(), json!(A2UI_MEDIA_TYPE))]));
        for part in [by_media, by_meta] {
            let m = Message::new(Role::User, vec![part]);
            let text = vymalo_inbound(&m).unwrap().payload["text"]
                .as_str()
                .unwrap()
                .to_owned();
            assert!(text.starts_with("The person used the interface"), "{text}");
        }
        // Without either, it is a plain data part: JSON text.
        let plain = Message::new(Role::User, vec![Part::data(action)]);
        let text = vymalo_inbound(&plain).unwrap().payload["text"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            text.starts_with("[{") && text.contains("\"action\""),
            "{text}"
        );
    }

    #[test]
    fn a2ui_messages_that_are_not_actions_say_nothing_and_text_beside_them_stays() {
        let m = Message::new(
            Role::User,
            vec![
                Part::text("hello"),
                a2ui_part(
                    json!([{"version": "v0.9.1", "createSurface": {"surfaceId": "s", "catalogId": "c"}}]),
                ),
            ],
        );
        assert_eq!(vymalo_inbound(&m).unwrap().payload["text"], "hello");
        let only = Message::new(
            Role::User,
            vec![a2ui_part(
                json!([{"version": "v0.9.1", "createSurface": {}}]),
            )],
        );
        assert!(vymalo_inbound(&only).is_err());
    }

    // ---- context ----

    fn ref_metadata(version: Value, inline: bool) -> Value {
        json!({UI_CATALOG_EXTENSION: {
            "catalogId": CATALOG_ID, "version": version, "digest": DIGEST, "inline": inline}})
    }

    #[test]
    fn a_reference_without_the_catalog_is_the_ref_only_and_the_version_is_an_integer() {
        let m = with_metadata(text_message("hi"), ref_metadata(json!(2.0), false));
        let payload = vymalo_inbound(&m).unwrap().payload;
        assert_eq!(
            payload["context"],
            json!({CONTEXT_UI_REF: {"catalogId": CATALOG_ID, "version": 2, "digest": DIGEST}})
        );
        assert!(payload["context"]["vymalo.ui.ref"]["version"].is_i64());
    }

    #[test]
    fn an_inline_catalog_is_kept_with_its_whole_numbers_as_integers_under_each_capability_key() {
        for (key, version) in [
            ("a2uiClientCapabilities", "v0.9.1"),
            ("a2uiClientCapabilities", "v0.9"),
            ("a2uiRendererCapabilities", "v1.0"),
        ] {
            let mut metadata = ref_metadata(json!(2), true);
            metadata[key] = json!({version: {
                "supportedCatalogIds": [CATALOG_ID, "https://a2ui.org/basic"],
                "inlineCatalogs": [catalog()]}});
            let m = with_metadata(text_message("hi"), metadata);
            let context = vymalo_inbound(&m).unwrap().payload["context"].clone();
            let kept = &context[CONTEXT_UI_CATALOG];
            assert_eq!(kept["digest"], DIGEST, "{key}/{version}");
            assert_eq!(kept["version"], 2);
            let max = &kept["catalog"]["components"]["Text"]["properties"]["text"]["maxLength"];
            assert!(max.is_i64() && *max == 4000, "{key}/{version}: {max}");
            assert!(context[CONTEXT_UI_REF].is_object());
        }
    }

    #[test]
    fn an_inline_catalog_of_another_id_is_ignored() {
        let mut metadata = ref_metadata(json!(2), true);
        let mut other = catalog();
        other["catalogId"] = json!("https://example.com/other");
        metadata["a2uiClientCapabilities"] =
            json!({"v0.9.1": {"inlineCatalogs": [other], "supportedCatalogIds": []}});
        let m = with_metadata(text_message("hi"), metadata);
        let context = vymalo_inbound(&m).unwrap().payload["context"].clone();
        assert!(context.get(CONTEXT_UI_CATALOG).is_none());
        assert!(context.get(CONTEXT_UI_REF).is_some());
    }

    #[test]
    fn a_catalog_without_the_extension_metadata_is_not_ours_and_is_not_read() {
        let mut metadata = json!({});
        metadata["a2uiClientCapabilities"] =
            json!({"v0.9.1": {"inlineCatalogs": [catalog()], "supportedCatalogIds": []}});
        let m = with_metadata(text_message("hi"), metadata);
        assert!(vymalo_inbound(&m).unwrap().payload.get("context").is_none());
    }

    #[test]
    fn a_malformed_reference_is_ignored() {
        for bad in [
            json!({"catalogId": CATALOG_ID, "version": 2, "digest": ""}),
            json!({"catalogId": CATALOG_ID, "version": 2.5, "digest": DIGEST}),
            json!({"catalogId": CATALOG_ID, "version": 0, "digest": DIGEST}),
            json!({"catalogId": CATALOG_ID, "version": "2", "digest": DIGEST}),
            json!({"version": 2, "digest": DIGEST}),
            json!("nope"),
        ] {
            let m = with_metadata(text_message("hi"), json!({UI_CATALOG_EXTENSION: bad}));
            assert!(
                vymalo_inbound(&m).unwrap().payload.get("context").is_none(),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_thread_tools_grant_is_copied_exactly_and_a_partial_one_is_ignored() {
        let grant = json!({
            "url": "http://orchestrator:8080/thread-tools/1b4e28ba/mcp",
            "token": "aaa.bbb.ccc",
            "expiresAt": "2026-10-01T14:00:00Z"});
        let m = with_metadata(text_message("hi"), json!({THREAD_TOOLS_EXTENSION: grant}));
        assert_eq!(
            vymalo_inbound(&m).unwrap().payload["context"],
            json!({CONTEXT_THREAD_TOOLS: grant})
        );
        for missing in ["url", "token", "expiresAt"] {
            let mut partial = grant.clone();
            partial.as_object_mut().unwrap().remove(missing);
            let m = with_metadata(text_message("hi"), json!({THREAD_TOOLS_EXTENSION: partial}));
            assert!(vymalo_inbound(&m).unwrap().payload.get("context").is_none());
        }
    }

    #[test]
    fn a_message_without_extensions_has_no_context() {
        let m = with_metadata(text_message("hi"), json!({"something": {"else": true}}));
        assert!(vymalo_inbound(&m).unwrap().payload.get("context").is_none());
        assert!(
            vymalo_inbound(&text_message("hi"))
                .unwrap()
                .payload
                .get("context")
                .is_none()
        );
    }

    #[test]
    fn integral_numbers_writes_whole_doubles_as_integers_and_leaves_the_rest() {
        let mut v = json!({"a": [1.0, 2, 2.5, -3.0], "b": {"c": 9007199254740992.0, "d": "1.0"}});
        integral_numbers(&mut v);
        assert!(v["a"][0].is_i64() && v["a"][0] == 1);
        assert!(v["a"][1].is_i64());
        assert!(v["a"][2].is_f64());
        assert!(v["a"][3].is_i64() && v["a"][3] == -3);
        assert!(v["b"]["c"].is_f64(), "beyond 2^53 - 1 stays a double");
        assert_eq!(v["b"]["d"], "1.0");
    }
}
