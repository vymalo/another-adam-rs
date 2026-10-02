//! The A2A extensions an agent that draws on a screen declares: the URIs, and the card entries.
//!
//! Seven extensions, each optional (a client that does not know one ignores it), detected by the
//! client from the card it reads, and removable without breaking plain A2A:
//!
//! | Extension | URI | What it is |
//! |---|---|---|
//! | A2UI v0.9.1 | [`A2UI_EXTENSION_V0_9_1`] | the agent can send A2UI surfaces (data parts of [`A2UI_MEDIA_TYPE`]) and receives the renderer's capabilities and actions |
//! | `ui-catalog/v1` | [`UI_CATALOG_EXTENSION`] | the agent reads the screen's own component catalog and draws with it |
//! | `thread-tools/v1` | [`THREAD_TOOLS_EXTENSION`] | the agent can use the per-thread tool endpoint a message announces |
//! | `steps/v1` | [`STEPS_EXTENSION`] | the agent reports its tool calls and its sub-agents' work as nested steps, to a client whose request activated it |
//! | `text-stream/v1` | [`TEXT_STREAM_EXTENSION`] | the agent sends its reply as the model writes it (chunks, transient) and says the whole text once, to a client whose request activated it |
//! | `mentions/v1` | [`MENTIONS_EXTENSION`] | the agent reads the agents a person mentioned in a message, and asks them (with the tool `ask_agent` of `thread-tools/v1`) |
//! | `steer/v1` | [`STEER_EXTENSION`] | a message that names a running task and activates the extension is added to that task's input, and the agent reads it at its next step |
//!
//! The contracts are the orchestration layer's (`docs/api/ui-catalog-v1.md`,
//! `docs/api/thread-tools-v1.md`, `docs/api/steps-v1.md`, `docs/api/text-stream-v1.md`,
//! `docs/api/mentions-v1.md` and `docs/api/steer-v1.md` of `vymalo/another-agentic-system`); what an agent does with the messages is `adam-a2a-runtime`'s
//! `vymalo_inbound` and `adam-ui`, and what it reports is `adam-a2a-runtime`'s subscription.

use serde_json::json;

use crate::card::ExtensionConfig;

/// The media type of a data part that carries A2UI messages, in `mediaType` (A2A 1.0) and in the
/// part's `metadata.mimeType` (the A2UI extension's own spelling).
pub const A2UI_MEDIA_TYPE: &str = "application/a2ui+json";

/// The URI of the A2UI v0.9.1 extension.
pub const A2UI_EXTENSION_V0_9_1: &str = "https://a2ui.org/a2a-extension/a2ui/v0.9.1";

/// The id of A2UI v0.9.1's basic catalog.
pub const A2UI_BASIC_CATALOG_V0_9_1: &str =
    "https://a2ui.org/specification/v0_9_1/catalogs/basic/catalog.json";

/// The URI of the `ui-catalog/v1` extension: the agent reads the screen's component catalog.
pub const UI_CATALOG_EXTENSION: &str = "https://agents.vymalo.com/a2a/extensions/ui-catalog/v1";

/// The URI of the `thread-tools/v1` extension: a message carries the endpoint of the tools of its
/// thread, and a token to call it.
pub const THREAD_TOOLS_EXTENSION: &str = "https://agents.vymalo.com/a2a/extensions/thread-tools/v1";

/// The URI of the `steps/v1` extension: the agent reports its work as nested steps, in the metadata
/// of `working` status messages, to a client whose request activated the extension.
pub const STEPS_EXTENSION: &str = "https://agents.vymalo.com/a2a/extensions/steps/v1";

/// The URI of the `text-stream/v1` extension: the agent sends its reply as the model writes it, as
/// artifact chunks (transient), and says the whole text once, in the metadata of a status message,
/// to a client whose request activated the extension.
pub const TEXT_STREAM_EXTENSION: &str = "https://agents.vymalo.com/a2a/extensions/text-stream/v1";

/// The URI of the `mentions/v1` extension: a message carries the agents the person mentioned in it
/// (`agentId`, label, position in the text), in the metadata under this URI.
pub const MENTIONS_EXTENSION: &str = "https://agents.vymalo.com/a2a/extensions/mentions/v1";

/// The URI of the `steer/v1` extension: a message that names a `submitted` or `working` task and
/// activates this extension is added to that task's input, and the agent reads it at its next step.
/// Without the activation such a message is refused, as plain A2A leaves it undefined.
pub const STEER_EXTENSION: &str = "https://agents.vymalo.com/a2a/extensions/steer/v1";

impl ExtensionConfig {
    /// The A2UI v0.9.1 extension, as the card of an agent that takes the screen's catalog inline
    /// declares it: `supportedCatalogIds` lists the basic catalog, and `acceptsInlineCatalogs` is
    /// `true` (A2UI's default is `false`, so without it a renderer never sends one).
    pub fn a2ui_v0_9_1() -> Self {
        let mut extension = Self::new(A2UI_EXTENSION_V0_9_1);
        extension.description =
            Some("Draws A2UI surfaces; takes the screen's catalog inline".into());
        extension.params.insert(
            "supportedCatalogIds".into(),
            json!([A2UI_BASIC_CATALOG_V0_9_1]),
        );
        extension
            .params
            .insert("acceptsInlineCatalogs".into(), json!(true));
        extension
    }

    /// The `ui-catalog/v1` extension: the agent reads the screen's component catalog and draws
    /// with it. Optional, no parameters.
    pub fn ui_catalog() -> Self {
        let mut extension = Self::new(UI_CATALOG_EXTENSION);
        extension.description = Some("Reads the screen's UI catalog and draws with it".into());
        extension
    }

    /// The `steps/v1` extension: the agent reports its tool calls and its sub-agents' work as
    /// nested steps. Optional, no parameters; a client that does not activate it gets plain text.
    pub fn steps() -> Self {
        let mut extension = Self::new(STEPS_EXTENSION);
        extension.description =
            Some("Reports its tool calls and its sub-agents' work as nested steps".into());
        extension
    }

    /// The `text-stream/v1` extension: the agent sends its reply as the model writes it, and a
    /// client that does not activate it gets the whole reply at the end, as it always did.
    /// Optional, no parameters.
    pub fn text_stream() -> Self {
        let mut extension = Self::new(TEXT_STREAM_EXTENSION);
        extension.description = Some("Streams its replies as they are written".into());
        extension
    }

    /// The `thread-tools/v1` extension: the agent calls the tools of the per-thread endpoint a
    /// message announces. Optional, no parameters.
    pub fn thread_tools() -> Self {
        let mut extension = Self::new(THREAD_TOOLS_EXTENSION);
        extension.description = Some("Calls the tools of the thread's endpoint".into());
        extension
    }

    /// The `steer/v1` extension: the agent reads a message sent to its running task at its next
    /// step. Optional, no parameters; the description is the contract's. Declaring it is a promise
    /// that an accepted message is never lost (see `docs/api/steer-v1.md`).
    pub fn steer() -> Self {
        let mut extension = Self::new(STEER_EXTENSION);
        extension.description =
            Some("Reads a message sent to its running task at its next step.".into());
        extension
    }

    /// The `mentions/v1` extension: the agent reads the agents a person mentioned in a message and
    /// asks them. Optional, no parameters; the description is the contract's.
    pub fn mentions() -> Self {
        let mut extension = Self::new(MENTIONS_EXTENSION);
        extension.description =
            Some("Reads the agents a person mentioned in a message, and asks them.".into());
        extension
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_uris_are_the_ones_of_the_contracts() {
        assert_eq!(
            UI_CATALOG_EXTENSION,
            "https://agents.vymalo.com/a2a/extensions/ui-catalog/v1"
        );
        assert_eq!(
            THREAD_TOOLS_EXTENSION,
            "https://agents.vymalo.com/a2a/extensions/thread-tools/v1"
        );
        assert_eq!(
            STEPS_EXTENSION,
            "https://agents.vymalo.com/a2a/extensions/steps/v1"
        );
        assert_eq!(
            TEXT_STREAM_EXTENSION,
            "https://agents.vymalo.com/a2a/extensions/text-stream/v1"
        );
        assert_eq!(
            MENTIONS_EXTENSION,
            "https://agents.vymalo.com/a2a/extensions/mentions/v1"
        );
        assert_eq!(
            STEER_EXTENSION,
            "https://agents.vymalo.com/a2a/extensions/steer/v1"
        );
        assert_eq!(
            A2UI_EXTENSION_V0_9_1,
            "https://a2ui.org/a2a-extension/a2ui/v0.9.1"
        );
        assert_eq!(A2UI_MEDIA_TYPE, "application/a2ui+json");
    }

    #[test]
    fn the_a2ui_entry_declares_inline_catalogs_and_the_basic_one() {
        let e = ExtensionConfig::a2ui_v0_9_1();
        assert_eq!(e.uri, A2UI_EXTENSION_V0_9_1);
        assert!(!e.required);
        assert_eq!(
            serde_json::Value::Object(e.params),
            json!({
                "supportedCatalogIds": [A2UI_BASIC_CATALOG_V0_9_1],
                "acceptsInlineCatalogs": true
            })
        );
    }

    #[test]
    fn the_vymalo_entries_are_optional_and_have_no_parameters() {
        for e in [
            ExtensionConfig::ui_catalog(),
            ExtensionConfig::thread_tools(),
            ExtensionConfig::steps(),
            ExtensionConfig::text_stream(),
            ExtensionConfig::mentions(),
            ExtensionConfig::steer(),
        ] {
            assert!(!e.required, "{}", e.uri);
            assert!(e.params.is_empty(), "{}", e.uri);
            assert!(e.description.is_some(), "{}", e.uri);
        }
        assert_eq!(ExtensionConfig::ui_catalog().uri, UI_CATALOG_EXTENSION);
        assert_eq!(ExtensionConfig::thread_tools().uri, THREAD_TOOLS_EXTENSION);
        assert_eq!(ExtensionConfig::steps().uri, STEPS_EXTENSION);
        assert_eq!(ExtensionConfig::text_stream().uri, TEXT_STREAM_EXTENSION);
        assert_eq!(ExtensionConfig::mentions().uri, MENTIONS_EXTENSION);
        assert_eq!(ExtensionConfig::steer().uri, STEER_EXTENSION);
    }
}
