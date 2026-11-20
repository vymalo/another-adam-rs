//! An artifact as a run's view carries it: a file by reference, not by bytes.
//!
//! A v4 run keeps a file artifact's bytes in its state, so its view carries them inline
//! ([`FileSource::Inline`]). A v5 head names the file's content address and size instead
//! ([`FileSource::Blob`]), and a reader fetches the bytes with
//! [`Runtime::artifact_bytes`](crate::Runtime::artifact_bytes). [`ArtifactRef`] is the one shape a
//! view uses for both, so a consumer does not have to know which format the run is in.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use adam_core::BlobId;

use crate::blob::blob_id;
use crate::events::{Artifact, base64_bytes};

/// Where a file artifact's bytes are.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileSource {
    /// The bytes, as a v4 run's state carries them.
    Inline(#[serde(with = "base64_bytes")] Vec<u8>),
    /// The content address, as a v5 head carries it: fetch the bytes with
    /// [`Runtime::artifact_bytes`](crate::Runtime::artifact_bytes).
    Blob(BlobId),
}

/// A file artifact of a run, by reference: its name, its size and where its bytes are.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRef {
    /// The name the file is saved under.
    pub filename: String,
    /// Its size in bytes, known before the bytes are fetched.
    pub size: u64,
    /// Where the bytes are.
    pub source: FileSource,
}

impl FileRef {
    /// A file whose bytes the state carries.
    pub fn inline(filename: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            filename: filename.into(),
            size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            source: FileSource::Inline(bytes),
        }
    }

    /// A file the head names by content address.
    pub fn blob(filename: impl Into<String>, size: u64, digest: BlobId) -> Self {
        Self {
            filename: filename.into(),
            size,
            source: FileSource::Blob(digest),
        }
    }

    /// The SHA-256 of the bytes: the blob's id for a [`Blob`](FileSource::Blob), the digest of
    /// the inline bytes otherwise. An inline file is hashed here; the view's own cache of derived
    /// artifact ids is what keeps a poll from hashing the same file twice.
    pub fn digest(&self) -> BlobId {
        match &self.source {
            FileSource::Inline(bytes) => blob_id(bytes),
            FileSource::Blob(digest) => digest.clone(),
        }
    }
}

/// An artifact of a run as a view carries it: a JSON artifact's data, a file artifact by reference.
///
/// The same shape as [`Artifact`], with `file` a [`FileRef`] instead of the bytes, so a reader of a
/// v5 run never transfers a file it does not serve.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ArtifactRef {
    /// The artifact's identity within the run, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Artifact name.
    pub name: String,
    /// Media type of `data` (or of the file), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// The content of a JSON artifact; `null` for a file.
    pub data: Value,
    /// The file, for a file artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<FileRef>,
}

impl ArtifactRef {
    /// A JSON artifact: `data` with its media type, if it has one.
    pub fn new(name: impl Into<String>, mime_type: Option<String>, data: Value) -> Self {
        Self {
            id: None,
            name: name.into(),
            mime_type,
            data,
            file: None,
        }
    }

    /// Keep this artifact under `id`: a later artifact of the run with the same id replaces it. A
    /// file artifact stays without one (files are content-addressed).
    #[must_use]
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        if self.file.is_none() {
            self.id = Some(id.into());
        }
        self
    }

    /// A file artifact whose bytes the state carries: the v4 shape, and what a run with no head
    /// keeps. The checks are [`Artifact::file`]'s.
    ///
    /// # Errors
    ///
    /// [`ArtifactFileError`](crate::ArtifactFileError): the file is over the limit, the filename
    /// is not a name, or the media type is not `type/subtype`.
    pub fn file(
        name: impl Into<String>,
        media_type: impl Into<String>,
        filename: impl Into<String>,
        bytes: Vec<u8>,
    ) -> Result<Self, crate::ArtifactFileError> {
        Ok(Self::inline(&Artifact::file(
            name, media_type, filename, bytes,
        )?))
    }

    /// `artifact` with its file's bytes inline: what a v4 run's state carries.
    pub fn inline(artifact: &Artifact) -> Self {
        Self {
            id: artifact.id.clone(),
            name: artifact.name.clone(),
            mime_type: artifact.mime_type.clone(),
            data: artifact.data.clone(),
            file: artifact
                .file
                .as_ref()
                .map(|file| FileRef::inline(file.filename.clone(), file.bytes.clone())),
        }
    }
}

impl From<&Artifact> for ArtifactRef {
    fn from(artifact: &Artifact) -> Self {
        Self::inline(artifact)
    }
}

impl From<Artifact> for ArtifactRef {
    fn from(artifact: Artifact) -> Self {
        Self {
            id: artifact.id,
            name: artifact.name,
            mime_type: artifact.mime_type,
            data: artifact.data,
            file: artifact
                .file
                .map(|file| FileRef::inline(file.filename, file.bytes)),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_v4_file_becomes_an_inline_ref_and_a_v5_one_a_blob_ref() {
        let artifact = Artifact::file("chart", "image/svg+xml", "chart.svg", b"<svg/>".to_vec())
            .expect("a file");
        let inline = ArtifactRef::inline(&artifact);
        let file = inline.file.as_ref().expect("a file");
        assert_eq!(file.filename, "chart.svg");
        assert_eq!(file.size, 6);
        assert_eq!(file.source, FileSource::Inline(b"<svg/>".to_vec()));
        assert_eq!(file.digest(), blob_id(b"<svg/>"));

        let digest = blob_id(b"<svg/>");
        let blob = FileRef::blob("chart.svg", 6, digest.clone());
        assert_eq!(blob.digest(), digest);
        assert_eq!(blob.size, 6);
    }

    #[test]
    fn an_artifact_ref_round_trips_through_json() {
        let artifact = ArtifactRef::inline(
            &Artifact::file("chart", "image/svg+xml", "chart.svg", b"<svg/>".to_vec())
                .expect("a file"),
        );
        let encoded = serde_json::to_value(&artifact).expect("encodes");
        assert_eq!(encoded["file"]["source"]["Inline"], json!("PHN2Zy8+"));
        let back: ArtifactRef = serde_json::from_value(encoded).expect("decodes");
        assert_eq!(back, artifact);
    }
}
