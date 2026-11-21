//! Which catalog the screen has right now, for a run: from the cache, from the message that
//! carried it, or read again from the thread-tools endpoint.
//!
//! ```mermaid
//! sequenceDiagram
//!     participant T as a tool (ask_user, show, ui_catalog)
//!     participant R as UiState::resolve
//!     participant C as CatalogCache
//!     participant E as thread tools: get_ui_catalog
//!     T->>R: the run's inbound context
//!     R->>R: does the renderer offer the current catalogId (vymalo.a2ui.renderer)?
//!     R->>R: the current digest (vymalo.ui.ref, else the inline catalog's)
//!     R->>C: a catalog with that digest?
//!     alt not offered
//!         R-->>T: NotOffered (answer in text)
//!     else cached
//!         C-->>T: the catalog
//!     else the message carried that catalog inline
//!         R->>R: read it, check its digest, keep it
//!         R-->>T: the catalog
//!     else stale (this copy is not the current one)
//!         R->>E: get_ui_catalog (Bearer grant), once
//!         E-->>R: the newest catalog, or an error
//!         R-->>T: the catalog, or Unreadable (the tool degrades)
//!     end
//! ```
//!
//! ```mermaid
//! stateDiagram-v2
//!     [*] --> NotOffered: no renderer entry, or it does not list the catalog
//!     NotOffered --> [*]: answer in text
//!     [*] --> Unknown: no ref, no inline catalog (the screen sent none)
//!     [*] --> Current: ref digest cached
//!     [*] --> Inline: the message carried the catalog
//!     Inline --> Current: digest checked, cached
//!     [*] --> Stale: ref digest not held, no matching inline catalog
//!     Stale --> Current: refetched, digest checked, cached
//!     Stale --> Unreadable: no grant, expired grant, endpoint error, bad document
//!     Unknown --> [*]: answer in text
//!     Unreadable --> [*]: degrade, never fail the run
//! ```

use std::sync::Arc;

use adam_a2a_runtime::{CONTEXT_A2UI_RENDERER, CONTEXT_UI_CATALOG, CONTEXT_UI_REF};
use serde_json::{Map, Value};

use crate::cache::CatalogCache;
use crate::catalog::{Catalog, Claimed};
#[cfg(feature = "thread-tools")]
use crate::thread_tools::ThreadToolsClient;

/// Why a catalog cannot be read again when this process has no thread tools.
const NO_THREAD_TOOLS: &str = "it cannot be read again: this agent has no thread tools";

/// What this process holds of the screen's catalog, before it asks anyone.
enum Held {
    Found(Arc<Catalog>),
    /// The renderer did not offer the catalog (or any): nothing is drawn.
    NotOffered,
    NoCatalog,
    Stale,
}

/// What a run knows of the screen's catalog.
pub(crate) enum Resolved {
    /// The catalog, read and checked.
    Found(Arc<Catalog>),
    /// The message's renderer capabilities did not offer the current catalog, or there were none
    /// (this turn): the agent answers in text, whatever a cache or an earlier turn holds.
    NotOffered,
    /// The thread has no catalog: the screen sent none, so the agent answers in text.
    NoCatalog,
    /// There is one, and it could not be read now (and why, without any credential): the agent
    /// degrades to text and says nothing of the reason to the person.
    Unreadable(String),
}

/// What the tools of one agent share: the catalogs this process has read, and how to read the
/// current one again.
pub(crate) struct UiState {
    pub(crate) cache: CatalogCache,
    /// How to read the catalog again; none in a build without thread tools, which keeps only what a
    /// message carried.
    #[cfg(feature = "thread-tools")]
    pub(crate) client: Option<Arc<ThreadToolsClient>>,
}

/// `{catalogId, version, digest}` of a context entry.
fn claimed(entry: &Value) -> Option<Claimed> {
    let text = |key: &str| {
        entry
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    let version = entry
        .get("version")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .filter(|v| *v >= 1)?;
    Some(Claimed {
        catalog_id: text("catalogId")?.to_owned(),
        version,
        digest: text("digest")?.to_owned(),
    })
}

/// The catalog ids the renderer offered this turn (`vymalo.a2ui.renderer`); `None` without the entry.
fn offered_ids(context: &Map<String, Value>) -> Option<Vec<&str>> {
    let ids = context
        .get(CONTEXT_A2UI_RENDERER)?
        .get("catalogIds")?
        .as_array()?;
    Some(ids.iter().filter_map(Value::as_str).collect())
}

impl UiState {
    #[cfg(feature = "thread-tools")]
    pub(crate) fn new(client: Arc<ThreadToolsClient>) -> Self {
        Self {
            cache: CatalogCache::new(),
            client: Some(client),
        }
    }

    /// No way to read a catalog again: only the one a message carried is used.
    pub(crate) fn without_thread_tools() -> Self {
        Self {
            cache: CatalogCache::new(),
            #[cfg(feature = "thread-tools")]
            client: None,
        }
    }

    /// The catalog the screen has now, for a run with inbound context `context`.
    pub(crate) async fn resolve(&self, context: &Map<String, Value>) -> Resolved {
        match self.held(context) {
            Held::Found(catalog) => Resolved::Found(catalog),
            Held::NotOffered => Resolved::NotOffered,
            Held::NoCatalog => Resolved::NoCatalog,
            // Stale: this process does not hold the current catalog. Ask the endpoint for the newest.
            Held::Stale => self.refetch(context).await,
        }
    }

    /// The catalog the screen has now, **when this process holds it already** (in its cache, or in the
    /// message that carried it) and without asking anyone: the description of `show` is made at every
    /// model turn, and a turn must not pay a request, or repeat a failing one, for it. `None`: there
    /// is none, or it is not held yet; a tool that needs it reads it again with
    /// [`resolve`](Self::resolve), and it is held from then on.
    pub(crate) fn current_if_held(&self, context: &Map<String, Value>) -> Option<Arc<Catalog>> {
        match self.held(context) {
            Held::Found(catalog) => Some(catalog),
            Held::NotOffered | Held::NoCatalog | Held::Stale => None,
        }
    }

    /// What is known of the current catalog without a request.
    fn held(&self, context: &Map<String, Value>) -> Held {
        // This turn's offer comes first: with none, nothing is drawn, cached or not.
        let Some(offered) = offered_ids(context) else {
            return Held::NotOffered;
        };
        let inline = context.get(CONTEXT_UI_CATALOG);
        // What is current: the thread's reference when the message carried one; the inline catalog's
        // own claim otherwise.
        let Some(current) = context
            .get(CONTEXT_UI_REF)
            .and_then(claimed)
            .or_else(|| inline.and_then(claimed))
        else {
            return Held::NoCatalog;
        };
        if !offered.contains(&current.catalog_id.as_str()) {
            return Held::NotOffered;
        }
        if let Some(cached) = self.cache.get(&current.digest) {
            return Held::Found(cached);
        }
        // The message carried exactly this catalog.
        if let Some(entry) = inline
            && claimed(entry).as_ref() == Some(&current)
            && let Some(document) = entry.get("catalog")
        {
            match Catalog::from_document(document.clone(), &current) {
                Ok(catalog) => {
                    let catalog = Arc::new(catalog);
                    self.cache.insert(Arc::clone(&catalog));
                    return Held::Found(catalog);
                }
                Err(error) => {
                    tracing::warn!(%error, "the catalog a message carried cannot be used; reading it again");
                }
            }
        }
        Held::Stale
    }

    #[cfg(feature = "thread-tools")]
    fn keep(&self, catalog: Catalog) -> Resolved {
        let catalog = Arc::new(catalog);
        self.cache.insert(Arc::clone(&catalog));
        Resolved::Found(catalog)
    }

    #[cfg(not(feature = "thread-tools"))]
    #[allow(clippy::unused_async)] // the same signature as the build with thread tools
    async fn refetch(&self, _context: &Map<String, Value>) -> Resolved {
        Resolved::Unreadable(NO_THREAD_TOOLS.into())
    }

    #[cfg(feature = "thread-tools")]
    async fn refetch(&self, context: &Map<String, Value>) -> Resolved {
        let Some(client) = &self.client else {
            return Resolved::Unreadable(NO_THREAD_TOOLS.into());
        };
        let grant = match client.grant(context) {
            Ok(grant) => grant,
            Err(why) => {
                tracing::debug!(
                    reason = why.reason(),
                    "the screen's catalog cannot be read again"
                );
                return Resolved::Unreadable(format!("it cannot be read again: {}", why.reason()));
            }
        };
        let fetched = match client.fetch_catalog(&grant, None).await {
            Ok(fetched) => fetched,
            Err(error) => {
                tracing::warn!(%error, "the screen's catalog could not be read again");
                return Resolved::Unreadable(error);
            }
        };
        // The renderer's offer decides, not the endpoint: a catalog of an id this turn did not offer is
        // never drawn with, whatever it hashes to.
        if !offered_ids(context)
            .is_some_and(|ids| ids.contains(&fetched.claimed.catalog_id.as_str()))
        {
            tracing::warn!("the endpoint's catalog is not one the renderer offered this turn");
            return Resolved::NotOffered;
        }
        let Some(document) = fetched.document else {
            return Resolved::Unreadable(
                "the endpoint said the catalog is unchanged, but this process holds none".into(),
            );
        };
        match Catalog::from_document(document, &fetched.claimed) {
            Ok(catalog) => self.keep(catalog),
            Err(error) => {
                tracing::warn!(%error, "the catalog the endpoint gave cannot be used");
                Resolved::Unreadable(error.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A message that names the current catalog without carrying it, and an endpoint that has it.
    fn named_only() -> Map<String, Value> {
        let context = json!({
            CONTEXT_A2UI_RENDERER: {"catalogIds": ["https://example.test/catalog"]},
            CONTEXT_UI_REF: {"catalogId": "https://example.test/catalog", "version": 2, "digest": "sha256:00"},
            adam_a2a_runtime::CONTEXT_THREAD_TOOLS: {
                "url": "http://127.0.0.1:9/thread-tools", "token": "tok", "expiresAt": "2999-01-01T00:00:00Z"
            },
        });
        context.as_object().cloned().unwrap_or_default()
    }

    /// Without thread tools nothing is read again, in a build with the feature (a `Ui::embedded`)
    /// and in one without it, and the reason says so.
    #[tokio::test]
    async fn without_thread_tools_a_catalog_is_never_read_again() {
        let state = UiState::without_thread_tools();
        match state.resolve(&named_only()).await {
            Resolved::Unreadable(reason) => assert_eq!(reason, NO_THREAD_TOOLS),
            Resolved::Found(_) => panic!("a catalog was found with nothing to read it from"),
            Resolved::NoCatalog | Resolved::NotOffered => panic!("the message names a catalog"),
        }
        assert_eq!(state.cache.len(), 0);
    }
}
