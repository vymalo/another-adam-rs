//! File bytes as content-addressed blobs, and the upload a commit does before it lands.
//!
//! A file artifact's bytes are stored under their SHA-256 ([`BlobId`]) and the commit that first
//! names them references the digest ([`RunUpdate::blobs`]), so a store can keep the bytes outside
//! the run and collect them once no run references them (ADR 0055). This module owns the digest
//! rule — `adam-core` has no hash dependency — and the upload. A commit that writes a head
//! **gates** on it: [`upload_all`] refuses the split when a put fails, and [`upload_carried`]
//! leaves a file the head cannot name out of a carried head. A v4 commit stays fail-open: its
//! state carries the bytes inline, so a failed put costs the copy, not the run.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use adam_core::{BlobId, DynStore, StoreResult};
use sha2::{Digest as _, Sha256};

use crate::events::{Artifact, ArtifactFile};

/// The most bytes [`BlobCache`] keeps: 32 MiB, eight of the largest artifact a run may carry.
pub(crate) const MAX_CACHED_ARTIFACT_BYTES: usize = 32 * 1024 * 1024;

/// The file bytes a reader has fetched, by content address, least recently used first.
///
/// A view carries a file by reference, so a poll of a run whose file a subscriber already read
/// must not read the blob again: the cache answers it. It is bounded by
/// [`MAX_CACHED_ARTIFACT_BYTES`] and never caches a blob larger than the whole budget. A blob is
/// immutable and content-addressed, so an entry cannot be stale; the cost of the bound is a
/// re-read, never a wrong byte.
///
/// The bound is in **bytes, not entries**: many tiny blobs can fill the map with many entries, and
/// [`get`](Self::get)/[`put`](Self::put) scan the order deque linearly. A run's file artifacts are
/// at most a handful (the artifact bounds), so the scan is short in practice; a store of millions of
/// one-byte blobs would not be.
#[derive(Default)]
pub(crate) struct BlobCache {
    entries: HashMap<BlobId, Arc<[u8]>>,
    order: VecDeque<BlobId>,
    bytes: usize,
}

impl BlobCache {
    /// The bytes of `id` if they are cached, marked most recently used.
    pub(crate) fn get(&mut self, id: &BlobId) -> Option<Arc<[u8]>> {
        let bytes = self.entries.get(id)?.clone();
        if let Some(at) = self.order.iter().position(|held| held == id) {
            self.order.remove(at);
        }
        self.order.push_back(id.clone());
        Some(bytes)
    }

    /// Keep `bytes` under `id`, evicting the least recently used entries over the bound.
    pub(crate) fn put(&mut self, id: BlobId, bytes: Arc<[u8]>) {
        if bytes.len() > MAX_CACHED_ARTIFACT_BYTES {
            return;
        }
        if let Some(old) = self.entries.insert(id.clone(), bytes.clone()) {
            self.bytes = self.bytes.saturating_sub(old.len());
            if let Some(at) = self.order.iter().position(|held| held == &id) {
                self.order.remove(at);
            }
        }
        self.bytes += bytes.len();
        self.order.push_back(id);
        while self.bytes > MAX_CACHED_ARTIFACT_BYTES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(gone) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(gone.len());
            }
        }
    }

    /// How many entries are cached (a test's view of the bound).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    fn id(n: u8) -> BlobId {
        BlobId::from_sha256(&[n; 32])
    }

    fn bytes(n: usize) -> Arc<[u8]> {
        Arc::from(vec![0_u8; n])
    }

    #[test]
    fn the_cache_is_bounded_and_least_recently_used_first() {
        let quarter = MAX_CACHED_ARTIFACT_BYTES / 4;
        let mut cache = BlobCache::default();
        for n in 0..4 {
            cache.put(id(n), bytes(quarter));
        }
        assert_eq!(cache.len(), 4, "four quarters fit");
        // A read makes the first the most recent, so the second is the one evicted.
        assert!(cache.get(&id(0)).is_some());
        cache.put(id(4), bytes(quarter));
        assert_eq!(cache.len(), 4);
        assert!(cache.get(&id(0)).is_some(), "the read entry stayed");
        assert!(cache.get(&id(1)).is_none(), "the least recently used went");
        assert!(cache.get(&id(4)).is_some(), "the new entry is there");
    }

    #[test]
    fn a_blob_larger_than_the_whole_budget_is_not_cached() {
        let mut cache = BlobCache::default();
        cache.put(id(9), bytes(MAX_CACHED_ARTIFACT_BYTES + 1));
        assert_eq!(cache.len(), 0);
        assert!(cache.get(&id(9)).is_none());
    }
}

/// The content address of `bytes`: their SHA-256 as 64 lower-case hexadecimal digits.
pub fn blob_id(bytes: &[u8]) -> BlobId {
    let mut digest = [0_u8; 32];
    digest.copy_from_slice(&Sha256::digest(bytes));
    BlobId::from_sha256(&digest)
}

/// An owned, cloneable handle that stores bytes as content-addressed blobs from inside a
/// [`Ctx::step`](crate::Ctx::step), taken with [`Ctx::blob_writer`](crate::Ctx::blob_writer).
///
/// It **only stores** the bytes: the run references them when the agent names the ids with
/// [`Ctx::reference_blobs`](crate::Ctx::reference_blobs) after the step, so the commit that keeps
/// them records them ([`RunUpdate::blobs`](adam_core::RunUpdate::blobs)) and the bytes survive
/// until no run references them ([`Store::purge_blobs`](adam_core::Store::purge_blobs)). Naming
/// them after the step, not inside it, is what covers a replay: a recorded step does not run its
/// closure again, but its recorded ids are named all the same.
#[derive(Clone)]
pub struct BlobWriter {
    store: DynStore,
}

impl std::fmt::Debug for BlobWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobWriter")
            .field("keeps_blobs", &self.store.keeps_blobs())
            .finish()
    }
}

impl BlobWriter {
    pub(crate) fn new(store: DynStore) -> Self {
        Self { store }
    }

    /// Whether the store keeps blobs at all ([`Store::keeps_blobs`](adam_core::Store::keeps_blobs)).
    /// A store that does not refuses every [`put`](Self::put).
    pub fn keeps_blobs(&self) -> bool {
        self.store.keeps_blobs()
    }

    /// Store `bytes` under their content address ([`blob_id`]) and return it.
    ///
    /// Idempotent: bytes the store already holds are not sent again (one existence check), and a
    /// second put of the same id writes nothing.
    ///
    /// # Errors
    ///
    /// [`StoreError::InvalidInput`](adam_core::StoreError::InvalidInput) when the store keeps no
    /// blobs, or the store's own error when the put fails.
    pub async fn put(&self, bytes: &[u8]) -> StoreResult<BlobId> {
        let id = blob_id(bytes);
        if !self.store.keeps_blobs() {
            return Err(adam_core::StoreError::InvalidInput(
                "this store keeps no blobs".into(),
            ));
        }
        if !self.store.blob_stored(&id).await.unwrap_or(false) {
            self.store.put_blob(&id, bytes).await?;
        }
        Ok(id)
    }
}

/// What one [`upload_all`] pass found: the digests of the file artifacts (with their sizes) and
/// whether every one of them is stored.
pub(crate) struct Uploaded {
    /// The digests, without duplicates.
    pub ids: Vec<BlobId>,
    /// Each file artifact's digest and byte length, **in artifact order** (a file that appears
    /// twice appears twice), so a head can zip them without hashing the bytes again.
    pub files: Vec<(BlobId, u64)>,
    /// Whether every file's bytes are stored. A head may only name bytes that are there.
    pub all_stored: bool,
}

/// Upload every file artifact of `artifacts` (idempotent, digest-deduped) and report whether all of
/// them are stored.
///
/// Unlike [`upload_files`], a failed put is **not** ignored: a commit that writes a head must not
/// name a digest whose bytes are missing, so the caller writes the state only when `all_stored` is
/// false. A store that keeps no blobs cannot back a file reference at all: `all_stored` is false
/// when there is a file, and the pass uploads nothing.
pub(crate) async fn upload_all(store: &DynStore, artifacts: &[Artifact]) -> Uploaded {
    let mut uploaded = Uploaded {
        ids: Vec::new(),
        files: Vec::new(),
        all_stored: true,
    };
    let files: Vec<&ArtifactFile> = artifacts
        .iter()
        .filter_map(|artifact| artifact.file.as_ref())
        .collect();
    if files.is_empty() {
        return uploaded;
    }
    if !store.keeps_blobs() {
        uploaded.all_stored = false;
        return uploaded;
    }
    let mut seen = HashSet::new();
    for file in files {
        let id = blob_id(&file.bytes);
        uploaded.files.push((
            id.clone(),
            u64::try_from(file.bytes.len()).unwrap_or(u64::MAX),
        ));
        if !seen.insert(id.clone()) {
            continue;
        }
        // A file an earlier commit uploaded is not sent again: one indexed existence check.
        if store.blob_stored(&id).await.unwrap_or(false) {
            uploaded.ids.push(id);
            continue;
        }
        if let Err(error) = store.put_blob(&id, &file.bytes).await {
            tracing::warn!(
                blob = %id,
                error = %adam_error::report(&error),
                "a file artifact's bytes could not be stored; a head must not name them"
            );
            uploaded.all_stored = false;
        }
        uploaded.ids.push(id);
    }
    uploaded
}

/// Store the bytes of every file artifact in `artifacts` and return their digests, without
/// duplicates.
///
/// Nothing is uploaded to a store that keeps no blobs ([`Store::keeps_blobs`](adam_core::Store::keeps_blobs)),
/// and nothing at all when no artifact is a file, so the common step costs no round trip.
///
/// **Fail-open, for a v4 commit only**: an upload that fails is logged and its digest is still
/// referenced — the state names the artifact, so the transition is not lost. A later commit retries
/// it only if the artifact is emitted again; otherwise the reference outlives the bytes. A commit
/// that writes a head does not use this path: [`upload_all`] reports `all_stored: false` and its
/// caller refuses the split, and [`upload_carried`] leaves the file out of a carried head, so a
/// head never names bytes that are missing (ADR 0055).
pub(crate) async fn upload_files(store: &DynStore, artifacts: &[Artifact]) -> Vec<BlobId> {
    if !store.keeps_blobs() {
        return Vec::new();
    }
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for file in artifacts.iter().filter_map(|a| a.file.as_ref()) {
        let id = blob_id(&file.bytes);
        if !seen.insert(id.clone()) {
            continue;
        }
        // A file an earlier commit uploaded is not sent again.
        if !store.blob_stored(&id).await.unwrap_or(false)
            && let Err(error) = store.put_blob(&id, &file.bytes).await
        {
            tracing::warn!(
                blob = %id,
                error = %adam_error::report(&error),
                "a file artifact's bytes could not be stored; the run's state still carries them"
            );
        }
        ids.push(id);
    }
    ids
}

/// The digests among `refs` that the store does not hold, checked once each and logged.
///
/// An inherited file whose bytes are gone cannot be stored again: a head carries only its digest,
/// not its bytes. A commit that carries the file therefore leaves it out instead of refusing for
/// ever. A store error is returned — whether the bytes are there is unknown, so the commit is
/// retried rather than dropping an artifact that may still be readable.
pub(crate) async fn missing_blobs(
    store: DynStore,
    refs: Vec<BlobId>,
) -> StoreResult<HashSet<BlobId>> {
    let mut missing = HashSet::new();
    for id in refs {
        if missing.contains(&id) {
            continue;
        }
        if !store.blob_stored(&id).await? {
            tracing::warn!(
                blob = %id,
                "an inherited file artifact's bytes are not stored; the head leaves it out"
            );
            missing.insert(id);
        }
    }
    Ok(missing)
}

/// What one [`upload_carried`] pass found: the artifacts a carried head may name, and their digests.
pub(crate) struct Carried {
    /// The transition's artifacts the head may name: every JSON artifact, and a file whose bytes
    /// are stored.
    pub artifacts: Vec<Artifact>,
    /// The digests of the kept files, without duplicates.
    pub ids: Vec<BlobId>,
}

/// Upload the transition's file artifacts for a commit that **carries a head**, and keep only the
/// ones the head may name.
///
/// A carried head is written beside the tombstone, so it is the run's only copy of its artifacts:
/// a head must never name a digest whose bytes are not stored (ADR 0055). A failed put therefore
/// drops that **file** artifact (logged); a JSON artifact carries its data in the head and is kept.
/// A store that keeps no blobs cannot back a file reference at all, so every file is dropped.
pub(crate) async fn upload_carried(store: &DynStore, artifacts: &[Artifact]) -> Carried {
    let mut carried = Carried {
        artifacts: Vec::new(),
        ids: Vec::new(),
    };
    let mut seen = HashSet::new();
    for artifact in artifacts {
        let Some(file) = artifact.file.as_ref() else {
            carried.artifacts.push(artifact.clone());
            continue;
        };
        if !store.keeps_blobs() {
            tracing::warn!(
                name = %artifact.name,
                "a store that keeps no blobs cannot back a file artifact; the carried head leaves it out"
            );
            continue;
        }
        let id = blob_id(&file.bytes);
        let stored = store.blob_stored(&id).await.unwrap_or(false)
            || store.put_blob(&id, &file.bytes).await.is_ok();
        if !stored {
            tracing::warn!(
                blob = %id,
                name = %artifact.name,
                "a file artifact's bytes could not be stored; the carried head leaves it out"
            );
            continue;
        }
        if seen.insert(id.clone()) {
            carried.ids.push(id);
        }
        carried.artifacts.push(artifact.clone());
    }
    carried
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use adam_core::MemoryStore;

    use super::*;

    #[test]
    fn the_id_is_the_lower_case_hex_of_the_sha256() {
        // The digest of "abc", from the SHA-256 standard.
        assert_eq!(
            blob_id(b"abc").as_str(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(blob_id(b"").as_str().len(), 64);
        assert_eq!(
            blob_id(b"").as_str(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            BlobId::from_hex(blob_id(b"abc").as_str()),
            Some(blob_id(b"abc"))
        );
    }

    #[tokio::test]
    async fn a_file_round_trips_through_a_store_by_its_id() {
        let store: DynStore = Arc::new(MemoryStore::new());
        let file = Artifact::file("chart", "image/svg+xml", "chart.svg", b"<svg/>".to_vec())
            .expect("a file artifact");
        let ids = upload_files(&store, std::slice::from_ref(&file)).await;
        assert_eq!(ids, vec![blob_id(b"<svg/>")]);
        assert_eq!(
            store.get_blob(&ids[0]).await.expect("get"),
            Some(b"<svg/>".to_vec())
        );
        // A JSON artifact uploads nothing, and the same file twice is one digest.
        let json = Artifact::new("report", None, serde_json::json!({"ok": true}));
        assert!(
            upload_files(&store, std::slice::from_ref(&json))
                .await
                .is_empty()
        );
        assert_eq!(upload_files(&store, &[file.clone(), file]).await.len(), 1);
    }

    #[tokio::test]
    async fn a_writer_stores_bytes_under_their_id_once() {
        let store: DynStore = Arc::new(MemoryStore::new());
        let writer = BlobWriter::new(store.clone());
        assert!(writer.keeps_blobs());
        let id = writer.put(b"prompt").await.expect("put");
        assert_eq!(id, blob_id(b"prompt"));
        assert_eq!(writer.put(b"prompt").await.expect("again"), id);
        assert_eq!(
            store.get_blob(&id).await.expect("get"),
            Some(b"prompt".to_vec())
        );
    }
}
