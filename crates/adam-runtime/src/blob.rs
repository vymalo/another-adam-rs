//! File bytes as content-addressed blobs, and the upload a commit does before it lands.
//!
//! A file artifact's bytes are stored under their SHA-256 ([`BlobId`]) and the commit that first
//! names them references the digest ([`RunUpdate::blobs`]), so a store can keep the bytes outside
//! the run and collect them once no run references them (ADR 0055). This module owns the digest
//! rule — `adam-core` has no hash dependency — and the upload, which is **fail-open in this
//! release**: a failed upload is logged and the commit goes on, because the run's state still
//! carries the bytes. The contract release gates a run's head on the upload instead.

use std::collections::HashSet;

use adam_core::{BlobId, DynStore};
use sha2::{Digest as _, Sha256};

use crate::events::{Artifact, ArtifactFile};

/// The content address of `bytes`: their SHA-256 as 64 lower-case hexadecimal digits.
pub fn blob_id(bytes: &[u8]) -> BlobId {
    let mut digest = [0_u8; 32];
    digest.copy_from_slice(&Sha256::digest(bytes));
    BlobId::from_sha256(&digest)
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
/// **Fail-open** (the expand release): an upload that fails is logged and its digest is still
/// referenced — the state names the artifact, so the transition is not lost. A later commit retries
/// it only if the artifact is emitted again; otherwise the reference outlives the bytes, which S2
/// must gate the head on (ADR 0055).
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
}
