//! Signing a run's head at commit and checking it on decode ([ADR 0068] decision 4, slice T3a-2).
//!
//! The runtime signs the document it commits as a run's state: the v5 head, or the v4 envelope of a
//! run whose state is not split. The payload is [`head_payload`]: `{typ, run, cnv, len, chain,
//! head}`. `len` and `chain` are the transcript's: for a v5 head the body rows it names, for a v4
//! state the agent's own transcript (`Agent::split`), and `head` is the document without the
//! members that are not part of the state:
//!
//! * `signature`, which is the thing signed;
//! * `seal`, the `{len, chain}` a v5 head carries so that a writer that does not touch the rows (a
//!   delivery, a cancel, a retry that does not split) signs again without reading them. A reader
//!   never trusts it: it recomputes both from the rows, so a wrong `seal` can only make the next
//!   check fail;
//! * `body.generation`, which the **store** resolves after the runtime signs (the runtime leaves it
//!   `null`), and which names where the rows are kept, not what the state is.
//!
//! What the signature adds to a row check is everything else: the inbox, the owed calls and every
//! other member of the agent's head, `rev`, the artifacts, and the run id and conversation. Rows are
//! chained as the store keeps them (a NUL is already replaced, ADR 0051).
//!
//! Without keys nothing here runs: no signature is written, none is required and an existing one is
//! not read (a write without keys drops it).
//!
//! [ADR 0068]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md

use adam_core::turn::{
    ChainHash, ConversationHash, DynTurnKeys, TurnKeyError, TurnKeys, TurnSignatureError,
    sign_head, turn_signature, verify_head,
};
use adam_core::{RunId, storable};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::runtime::RuntimeError;

/// What a v5 head carries of its transcript so a writer that leaves the rows alone can sign again:
/// how many rows, and the chain over them (`h(len - 1)`, base64url). Never trusted by a reader.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Seal {
    /// The number of body rows.
    pub len: u32,
    /// `h(len - 1)` as [`ChainHash`] writes it.
    pub chain: String,
}

impl Seal {
    /// The seal of a transcript.
    pub(crate) fn over(
        run: RunId,
        conversation: Option<&str>,
        rows: &[Value],
    ) -> Result<(Self, ChainHash), RuntimeError> {
        let (len, chain) = chain_over(run, conversation, rows)?;
        Ok((
            Self {
                len,
                chain: chain.to_string(),
            },
            chain,
        ))
    }

    /// The seal's `len` and chain, or `None` when the chain is not one.
    pub(crate) fn parts(&self) -> Option<(u32, ChainHash)> {
        ChainHash::parse(&self.chain).map(|chain| (self.len, chain))
    }
}

/// The length of `rows` and the chain over them, each as the store keeps it.
pub(crate) fn chain_over(
    run: RunId,
    conversation: Option<&str>,
    rows: &[Value],
) -> Result<(u32, ChainHash), RuntimeError> {
    let len = u32::try_from(rows.len()).map_err(|_| RuntimeError::Corrupt {
        run,
        reason: "a transcript has more rows than a head signature can name".into(),
        source: None,
    })?;
    let cnv = ConversationHash::of(conversation, run);
    let chain = rows.iter().fold(ChainHash::seed(run, &cnv), |chain, row| {
        chain.next(&storable(row))
    });
    Ok((len, chain))
}

/// The document a head signature covers: `doc` as the store keeps it, without `signature`, `seal`
/// and `body.generation`.
fn signed_head(doc: &Value) -> Value {
    let mut head = doc.clone();
    if let Some(map) = head.as_object_mut() {
        map.remove("signature");
        map.remove("seal");
        if let Some(Value::Object(body)) = map.get_mut("body") {
            body.remove("generation");
        }
    }
    storable(&head).into_owned()
}

/// Why a head is refused: a [`RuntimeError::Corrupt`] that names the case and holds nothing of the
/// state.
fn refused(run: RunId, reason: &str, cause: Option<TurnSignatureError>) -> RuntimeError {
    RuntimeError::Corrupt {
        run,
        reason: format!("the run's head is refused (ADR 0068): {reason}"),
        source: cause.map(|e| Box::new(e) as _),
    }
}

/// The keys a runtime signs its heads with and checks them against.
#[derive(Clone)]
pub(crate) struct Sealer {
    keys: DynTurnKeys,
}

impl Sealer {
    pub(crate) fn new(keys: DynTurnKeys) -> Self {
        Self { keys }
    }

    /// The signature of `doc` (a run's state document, whatever it carries of a signature) for a
    /// transcript of `len` rows with `chain`.
    pub(crate) async fn sign(
        &self,
        run: RunId,
        conversation: Option<&str>,
        len: u32,
        chain: &ChainHash,
        doc: &Value,
    ) -> Result<String, TurnKeyError> {
        let cnv = ConversationHash::of(conversation, run);
        sign_head(self.keys.as_ref(), run, &cnv, len, chain, &signed_head(doc)).await
    }

    /// Check the signature `doc` carries against the run, its conversation, the transcript
    /// (`len`, `chain`) and everything else in `doc`.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Corrupt`] (class `Corrupt`) for a document that is unsigned, whose signature
    /// is not a string, or that does not verify; [`RuntimeError::TurnKeys`] when the key set cannot
    /// be read.
    pub(crate) async fn check(
        &self,
        run: RunId,
        conversation: Option<&str>,
        len: u32,
        chain: &ChainHash,
        doc: &Value,
    ) -> Result<(), RuntimeError> {
        let jws = match doc.get("signature") {
            None => {
                return Err(refused(
                    run,
                    "it carries no signature",
                    Some(TurnSignatureError::Unsigned),
                ));
            }
            Some(Value::String(jws)) => jws,
            Some(_) => {
                return Err(refused(
                    run,
                    "its signature is not a string",
                    Some(TurnSignatureError::Malformed),
                ));
            }
        };
        let set = self.keys.key_set().await?;
        let cnv = ConversationHash::of(conversation, run);
        verify_head(&set, run, &cnv, len, chain, &signed_head(doc), jws)
            .map(drop)
            .map_err(|e| refused(run, "its signature does not verify", Some(e)))
    }

    /// Check the head of a record read with its rows: the rows are chained and the one signature
    /// is verified. A document with **no** signature is adopted (decision 11, release N) unless a
    /// row carries a turn signature, which a run that was signed once must keep.
    ///
    /// # Errors
    ///
    /// As [`check`](Self::check), and `Corrupt` for an unsigned head over signed turns.
    pub(crate) async fn check_read(
        &self,
        run: RunId,
        conversation: Option<&str>,
        rows: &[Value],
        doc: &Value,
    ) -> Result<(), RuntimeError> {
        if doc.get("signature").is_none() {
            if rows.iter().any(|row| turn_signature(row).is_some()) {
                return Err(refused(
                    run,
                    "it is unsigned over signed turns",
                    Some(TurnSignatureError::Unsigned),
                ));
            }
            return Ok(());
        }
        let (len, chain) = chain_over(run, conversation, rows)?;
        self.check(run, conversation, len, &chain, doc).await
    }
}

impl std::fmt::Debug for Sealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sealer").field("keys", &self.keys).finish()
    }
}

/// Put `signature` (and, for a v5 head, its `seal`) in `doc`, an encoded state document.
pub(crate) fn attach(doc: &mut Value, signature: String, seal: Option<&Seal>) {
    if let Some(map) = doc.as_object_mut() {
        map.insert("signature".into(), Value::String(signature));
        if let Some(seal) = seal
            && let Ok(seal) = serde_json::to_value(seal)
        {
            map.insert("seal".into(), seal);
        }
    }
}
