//! The request a model call was given, recorded as content-addressed blobs ([ADR 0085] decision 7).
//!
//! The model step stores the final request's system prompt (its UTF-8 bytes) and its tool list (the
//! RFC 8785 canonical JSON of the `Vec<ToolSpec>`, so equal lists are equal bytes) with
//! [`BlobWriter::put`], and journals their ids with the answer. The same prompt and tools are
//! therefore stored once across turns and runs, and a replay of a recorded call names the same ids
//! without reading anything. The run's state keeps the last call's ids
//! ([`Conversation::request`](crate::Conversation::request)); an exporter reads the bytes back with
//! [`RecordedRequest::load`].
//!
//! **Fail-open.** This is training metadata, not the run's truth: a store that keeps no blobs, or a
//! put that fails, never fails the model call. The turn records no request (logged once), and the
//! state's `request` is `None` for it.
//!
//! [ADR 0085]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0085-runs-are-exported-as-training-records.md

use std::sync::atomic::{AtomicBool, Ordering};

use adam_core::{BlobId, Store, StoreError, StoreResult};
use adam_model::{ModelRequest, ToolSpec};
use adam_runtime::{BlobWriter, blob_id};
use serde::{Deserialize, Serialize};

/// The system prompt and the tools of one model call, by content address.
///
/// Recorded by the model step of the turn [`turn`](Self::turn) and kept in
/// [`Conversation::request`](crate::Conversation::request) for the run's last call that recorded
/// one. Both blobs were stored before the call's answer was journaled (the puts run beside the
/// call), and the commit that keeps
/// the state references them ([`RunUpdate::blobs`](adam_core::RunUpdate::blobs)), so they survive
/// until no run references them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedRequest {
    /// The turn of the call ([`Conversation::turns`](crate::Conversation::turns) when it was made).
    pub turn: u32,
    /// The system prompt's UTF-8 bytes. `None` when the request had no system prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<BlobId>,
    /// The tools the request carried, as [`RecordedRequest::tools_bytes`] encodes them (an empty
    /// list is `[]`): decode with `serde_json::from_slice::<Vec<ToolSpec>>`.
    pub tools: BlobId,
}

/// What a [`RecordedRequest`] names, read back from the store.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestContent {
    /// The system prompt, exactly as sent; `None` when the request had none.
    pub system: Option<String>,
    /// The tools, exactly as sent, in the request's order.
    pub tools: Vec<ToolSpec>,
}

impl RecordedRequest {
    /// The bytes the tools of a request are stored as: the RFC 8785 canonical JSON
    /// ([`adam_core::canonicalize`]) of the list, so equal lists give equal bytes and one blob.
    ///
    /// # Errors
    ///
    /// A spec that does not serialize as JSON (it cannot: a `ToolSpec` is plain JSON).
    pub fn tools_bytes(tools: &[ToolSpec]) -> serde_json::Result<Vec<u8>> {
        Ok(adam_core::canonicalize(&serde_json::to_value(tools)?))
    }

    /// The blobs this record names: the system prompt's (when there is one) and the tools'.
    pub fn blob_ids(&self) -> impl Iterator<Item = BlobId> + '_ {
        self.system
            .iter()
            .cloned()
            .chain(std::iter::once(self.tools.clone()))
    }

    /// Read the system prompt and the tools back from `store`.
    ///
    /// `Ok(None)` when a blob is not there (never stored, or collected once no run referenced it).
    ///
    /// # Errors
    ///
    /// The store's error, or [`StoreError::Corrupt`] when the bytes are not what the model step
    /// writes (a prompt that is not UTF-8, tools that do not decode as a `Vec<ToolSpec>`).
    pub async fn load<S: Store + ?Sized>(&self, store: &S) -> StoreResult<Option<RequestContent>> {
        let system = match &self.system {
            None => None,
            Some(id) => {
                let Some(bytes) = store.get_blob(id).await? else {
                    return Ok(None);
                };
                Some(String::from_utf8(bytes).map_err(|_| {
                    StoreError::Corrupt(format!("the system prompt blob {id} is not UTF-8"))
                })?)
            }
        };
        let Some(bytes) = store.get_blob(&self.tools).await? else {
            return Ok(None);
        };
        let tools = serde_json::from_slice::<Vec<ToolSpec>>(&bytes).map_err(|e| {
            StoreError::Corrupt(format!(
                "the tools blob {} does not decode as tool specs: {e}",
                self.tools
            ))
        })?;
        Ok(Some(RequestContent { system, tools }))
    }
}

/// The bytes of a request to record, and their ids, made from the final request.
pub(crate) struct Pending {
    turn: u32,
    system: Option<(BlobId, Vec<u8>)>,
    tools: (BlobId, Vec<u8>),
}

impl Pending {
    /// The system prompt and tools of `request` (never its images: they are not in either).
    /// `None` when the store keeps no blobs: nothing is hashed for a request that cannot be stored.
    pub(crate) fn of(writer: &BlobWriter, turn: u32, request: &ModelRequest) -> Option<Self> {
        if !writer.keeps_blobs() {
            once(&NO_BLOBS, || {
                tracing::debug!(
                    "the store keeps no blobs: model requests are not recorded for export"
                );
            });
            return None;
        }
        let tools = match RecordedRequest::tools_bytes(&request.tools) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::debug!(%error, "the request's tools do not encode; the request is not recorded");
                return None;
            }
        };
        Some(Self {
            turn,
            system: request
                .system
                .as_ref()
                .map(|system| (blob_id(system.as_bytes()), system.as_bytes().to_vec())),
            tools: (blob_id(&tools), tools),
        })
    }

    /// Store what is not known yet and return the record, or `None` when a put failed (logged:
    /// a warning the first time in the process, then at debug).
    ///
    /// `known` is the record the run's state already has: a blob it names is stored and
    /// referenced by the run, so the same prompt or tools are not sent again on every turn.
    pub(crate) async fn store(
        self,
        writer: &BlobWriter,
        known: Option<&RecordedRequest>,
    ) -> Option<RecordedRequest> {
        let system = match self.system {
            None => None,
            Some((id, bytes)) => Some(put(writer, known, id, &bytes).await?),
        };
        let (id, bytes) = self.tools;
        let tools = put(writer, known, id, &bytes).await?;
        Some(RecordedRequest {
            turn: self.turn,
            system,
            tools,
        })
    }
}

/// Store `bytes` under `id` unless `known` already names it; `None` when the put failed (logged: a
/// warning the first time in the process, then at debug).
async fn put(
    writer: &BlobWriter,
    known: Option<&RecordedRequest>,
    id: BlobId,
    bytes: &[u8],
) -> Option<BlobId> {
    if known.is_some_and(|k| k.blob_ids().any(|held| held == id)) {
        return Some(id);
    }
    match writer.put(bytes).await {
        Ok(id) => Some(id),
        Err(error) => {
            let error = adam_error::report(&error);
            if PUT_FAILED.swap(true, Ordering::Relaxed) {
                tracing::debug!(blob = %id, %error, "a model request could not be recorded");
            } else {
                tracing::warn!(
                    blob = %id,
                    %error,
                    "a model request could not be recorded for export; the call goes on \
                     (further failures are logged at debug)"
                );
            }
            None
        }
    }
}

/// Whether the "keeps no blobs" line was logged in this process.
static NO_BLOBS: AtomicBool = AtomicBool::new(false);
/// Whether a failed put was warned about in this process.
static PUT_FAILED: AtomicBool = AtomicBool::new(false);

fn once(flag: &AtomicBool, log: impl FnOnce()) {
    if !flag.swap(true, Ordering::Relaxed) {
        log();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(name: &str, parameters: serde_json::Value) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: format!("the {name} tool"),
            parameters,
        }
    }

    #[test]
    fn equal_tool_lists_are_equal_bytes_and_decode_back() {
        let a = vec![spec(
            "read",
            json!({"type": "object", "properties": {"b": {}, "a": {}}}),
        )];
        let b: Vec<ToolSpec> = serde_json::from_str(
            r#"[{"parameters":{"properties":{"a":{},"b":{}},"type":"object"},"description":"the read tool","name":"read"}]"#,
        )
        .expect("specs");
        let bytes = RecordedRequest::tools_bytes(&a).expect("bytes");
        assert_eq!(bytes, RecordedRequest::tools_bytes(&b).expect("bytes"));
        assert_eq!(
            serde_json::from_slice::<Vec<ToolSpec>>(&bytes).expect("decode"),
            a
        );
        assert_eq!(RecordedRequest::tools_bytes(&[]).expect("bytes"), b"[]");
    }

    #[test]
    fn a_record_without_a_system_prompt_omits_it() {
        let record = RecordedRequest {
            turn: 2,
            system: None,
            tools: blob_id(b"[]"),
        };
        let value = serde_json::to_value(&record).expect("json");
        assert_eq!(
            value,
            json!({"turn": 2, "tools": blob_id(b"[]").to_string()})
        );
        assert_eq!(
            serde_json::from_value::<RecordedRequest>(value).expect("decode"),
            record
        );
        assert_eq!(record.blob_ids().collect::<Vec<_>>(), vec![blob_id(b"[]")]);
    }
}
