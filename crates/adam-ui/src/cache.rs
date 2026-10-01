//! [`CatalogCache`]: the catalogs this process has read, by digest.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use crate::catalog::Catalog;

/// How many catalogs a cache keeps. A thread has one current catalog and the cache is shared by
/// every thread the process serves; two UI versions in the field at once need two.
pub const MAX_CACHED_CATALOGS: usize = 8;

/// The catalogs this process has read (from a message that carried one, or from the thread-tools
/// endpoint), by digest, newest last; the oldest is dropped past [`MAX_CACHED_CATALOGS`].
///
/// A digest names exactly one document, so an entry is never stale: only absent. It is a cache
/// of *this process* and nothing else: a restart or another replica reads the catalog again from
/// the message or the endpoint, so nothing here has to survive.
#[derive(Debug, Default)]
pub struct CatalogCache {
    entries: Mutex<VecDeque<Arc<Catalog>>>,
}

impl CatalogCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// The catalog with this digest, if the process has read it.
    pub fn get(&self, digest: &str) -> Option<Arc<Catalog>> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|c| c.digest() == digest)
            .cloned()
    }

    /// Keep `catalog` (replacing one with the same digest, which is the same document).
    pub fn insert(&self, catalog: Arc<Catalog>) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|c| c.digest() != catalog.digest());
        entries.push_back(catalog);
        while entries.len() > MAX_CACHED_CATALOGS {
            entries.pop_front();
        }
    }

    /// How many catalogs are kept.
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether nothing is kept.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::catalog::{Claimed, catalog_digest};

    fn catalog(n: usize) -> Arc<Catalog> {
        let document = json!({
            "catalogId": "https://agents.vymalo.com/a2ui/catalogs/test",
            "components": {"Note": {"type": "object", "description": n.to_string()}}
        });
        let digest = catalog_digest(&document).unwrap();
        Arc::new(
            Catalog::from_document(
                document,
                &Claimed {
                    catalog_id: "https://agents.vymalo.com/a2ui/catalogs/test".into(),
                    version: 1,
                    digest,
                },
            )
            .unwrap(),
        )
    }

    #[test]
    fn a_catalog_is_found_by_digest_and_the_oldest_goes_first() {
        let cache = CatalogCache::new();
        assert!(cache.is_empty());
        let first = catalog(0);
        cache.insert(first.clone());
        assert!(Arc::ptr_eq(&cache.get(first.digest()).unwrap(), &first));
        assert!(cache.get("sha256:nope").is_none());
        for n in 1..=MAX_CACHED_CATALOGS {
            cache.insert(catalog(n));
        }
        assert_eq!(cache.len(), MAX_CACHED_CATALOGS);
        assert!(
            cache.get(first.digest()).is_none(),
            "the oldest was dropped"
        );
        // The same digest again is not a second entry.
        let last = catalog(MAX_CACHED_CATALOGS);
        cache.insert(last.clone());
        assert_eq!(cache.len(), MAX_CACHED_CATALOGS);
        assert!(cache.get(last.digest()).is_some());
    }
}
