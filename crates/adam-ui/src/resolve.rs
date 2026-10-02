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
//!     R->>R: the current digest (vymalo.ui.ref, else the inline catalog's)
//!     R->>C: a catalog with that digest?
//!     alt cached
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

use adam_a2a_runtime::{CONTEXT_UI_CATALOG, CONTEXT_UI_REF};
use serde_json::{Map, Value};

use crate::cache::CatalogCache;
use crate::catalog::{Catalog, Claimed};
use crate::thread_tools::ThreadToolsClient;

/// What this process holds of the screen's catalog, before it asks anyone.
enum Held {
    Found(Arc<Catalog>),
    NoCatalog,
    Stale,
}

/// What a run knows of the screen's catalog.
pub(crate) enum Resolved {
    /// The catalog, read and checked.
    Found(Arc<Catalog>),
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
    pub(crate) client: Arc<ThreadToolsClient>,
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

impl UiState {
    pub(crate) fn new(client: Arc<ThreadToolsClient>) -> Self {
        Self {
            cache: CatalogCache::new(),
            client,
        }
    }

    /// The catalog the screen has now, for a run with inbound context `context`.
    pub(crate) async fn resolve(&self, context: &Map<String, Value>) -> Resolved {
        match self.held(context) {
            Held::Found(catalog) => Resolved::Found(catalog),
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
            Held::NoCatalog | Held::Stale => None,
        }
    }

    /// What is known of the current catalog without a request.
    fn held(&self, context: &Map<String, Value>) -> Held {
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

    fn keep(&self, catalog: Catalog) -> Resolved {
        let catalog = Arc::new(catalog);
        self.cache.insert(Arc::clone(&catalog));
        Resolved::Found(catalog)
    }

    async fn refetch(&self, context: &Map<String, Value>) -> Resolved {
        let grant = match self.client.grant(context) {
            Ok(grant) => grant,
            Err(why) => {
                tracing::debug!(
                    reason = why.reason(),
                    "the screen's catalog cannot be read again"
                );
                return Resolved::Unreadable(format!("it cannot be read again: {}", why.reason()));
            }
        };
        let fetched = match self.client.fetch_catalog(&grant, None).await {
            Ok(fetched) => fetched,
            Err(error) => {
                tracing::warn!(%error, "the screen's catalog could not be read again");
                return Resolved::Unreadable(error);
            }
        };
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
