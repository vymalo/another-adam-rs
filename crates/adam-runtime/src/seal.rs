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
//! **The turns** ([`Sealer::check_turns`]): a signed head is not the whole check. Every assistant and
//! tool row of the rows a decode read must also carry a turn signature that verifies at its stored
//! position, over the chain of every row before it, with a key that is in the key set and not
//! revoked. The head signature already covers the bytes; the turn check is what refuses a turn
//! signed by a key that was revoked after the head was signed over it (a rotation), a run whose
//! turns the agent never signed, and, for a history that is carried into a new run, a turn that was
//! not adam's. A row is a turn when it is an object whose `role` is `assistant` or `tool`.
//!
//! Without keys nothing here runs: no signature is written, none is required and an existing one is
//! not read (a write without keys drops it).
//!
//! **The journal** ([`Sealer::sign_entry`], [`Sealer::check_entry`]): the runtime signs each entry
//! it records (`adam-step/v1`, the run and the entry's `seq`, `name`, `ok` and `payload`) and checks
//! every entry it replays, in `Ctx`. Entries are not chained: each is checked on its own, and one
//! with no signature is adopted in this release (decision 11).
//!
//! [ADR 0068]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md

use adam_core::turn::{
    ChainHash, ConversationHash, DynTurnKeys, KeySet, TurnKeyError, TurnKeys, TurnPosition,
    TurnSignatureError, sign_head, sign_step, turn_signature, verify_head, verify_step,
    verify_turn,
};
use adam_core::{JournalEntry, RunId, storable};
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

/// The signature text of a head, or the refusal of one that carries none or no string.
fn head_signature(run: RunId, doc: &Value) -> Result<&str, RuntimeError> {
    match doc.get("signature") {
        None => Err(refused(
            run,
            "it carries no signature",
            Some(TurnSignatureError::Unsigned),
        )),
        Some(Value::String(jws)) => Ok(jws),
        Some(_) => Err(refused(
            run,
            "its signature is not a string",
            Some(TurnSignatureError::Malformed),
        )),
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
        let jws = head_signature(run, doc)?;
        let set = self.keys.key_set().await?;
        Self::verify_head_with(&set, run, conversation, len, chain, doc, jws)
    }

    /// The head check against a key set the caller already read, so a decode that also checks the
    /// turns reads the set once.
    fn verify_head_with(
        set: &KeySet,
        run: RunId,
        conversation: Option<&str>,
        len: u32,
        chain: &ChainHash,
        doc: &Value,
        jws: &str,
    ) -> Result<(), RuntimeError> {
        let cnv = ConversationHash::of(conversation, run);
        verify_head(set, run, &cnv, len, chain, &signed_head(doc), jws)
            .map(drop)
            .map_err(|e| refused(run, "its signature does not verify", Some(e)))
    }

    /// Refuse to sign over a history whose assistant or tool rows are not signed: a runtime with
    /// turn keys and an agent that does not sign its turns (`LlmAgent::turn_keys`) would otherwise
    /// commit a signed head that its own next decode refuses for good. Only the presence of a turn
    /// signature is checked here (a decode verifies each one in full); no key is read.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::UnsignedTurns`] (class `Invalid`, permanent: a configuration the same run
    /// never recovers from) naming the first row that is a turn and carries no signature.
    pub(crate) fn require_signed_turns(run: RunId, rows: &[Value]) -> Result<(), RuntimeError> {
        match rows
            .iter()
            .enumerate()
            .find(|(_, row)| is_turn(row) && turn_signature(row).is_none())
        {
            None => Ok(()),
            Some((row, value)) => Err(RuntimeError::UnsignedTurns {
                run,
                row,
                role: role_of(value).to_owned(),
            }),
        }
    }

    /// The `{len, chain}` a **signed** v5 head's `seal` carries, or `Corrupt`: a head with a
    /// signature and no usable seal cannot be checked without its rows, and a writer that does not
    /// read them must not sign over it.
    pub(crate) fn seal_parts(
        &self,
        run: RunId,
        seal: Option<&Seal>,
    ) -> Result<(u32, ChainHash), RuntimeError> {
        seal.and_then(Seal::parts)
            .ok_or_else(|| refused(run, "it is signed and carries no usable seal", None))
    }

    /// Check a v5 head against the `{len, chain}` of its own `seal`, without reading a row: what a
    /// writer that does not touch the transcript, and a reader of a head only, can verify. The
    /// signature covers `len` and `chain`, so a forged seal cannot make a forged head verify; what
    /// it does not check is that the rows still chain to it. A head with **no** signature is left
    /// to the caller (adoption needs the rows).
    ///
    /// # Errors
    ///
    /// As [`check`](Self::check), and `Corrupt` for a signed head with no usable seal.
    pub(crate) async fn check_sealed(
        &self,
        run: RunId,
        conversation: Option<&str>,
        seal: Option<&Seal>,
        doc: &Value,
    ) -> Result<(), RuntimeError> {
        let (len, chain) = self.seal_parts(run, seal)?;
        self.check(run, conversation, len, &chain, doc).await
    }

    /// The signature of a journal entry the runtime is about to write (`adam-step/v1`, ADR 0068
    /// decision 2): the detached JWS of the entry's `{typ, run, seq, name, ok, payload}`.
    ///
    /// # Errors
    ///
    /// [`TurnKeyError`] from the keys.
    pub(crate) async fn sign_entry(
        &self,
        run: RunId,
        entry: &JournalEntry,
    ) -> Result<String, TurnKeyError> {
        sign_step(self.keys.as_ref(), run, entry).await
    }

    /// Check the signature a journal entry the store returned carries. An entry with **no**
    /// signature is adopted (ADR 0068 decision 11, release N): a journal written before the feature
    /// replays as it is. A signed one must verify, and an entry altered in the store does not.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Corrupt`] (class `Corrupt`) naming the entry's `seq` for one that does not
    /// verify; [`RuntimeError::TurnKeys`] when the key set cannot be read.
    pub(crate) async fn check_entry(
        &self,
        run: RunId,
        entry: &JournalEntry,
    ) -> Result<(), RuntimeError> {
        if entry.signature.is_none() {
            return Ok(());
        }
        let set = self.keys.key_set().await?;
        verify_step(&set, run, entry)
            .map(drop)
            .map_err(|cause| RuntimeError::Corrupt {
                run,
                reason: format!(
                    "the journal entry {} is refused (ADR 0068): {cause}",
                    entry.seq
                ),
                source: Some(Box::new(cause)),
            })
    }

    /// Check the head of a record read with its rows: the rows are chained and the one signature
    /// is verified. A document with **no** signature is adopted (decision 11, release N) unless a
    /// row carries a turn signature, which a run that was signed once must keep.
    ///
    /// # Errors
    ///
    /// As [`check`](Self::check), and `Corrupt` for an unsigned head over signed turns. A **signed**
    /// head is then followed by [`check_turns`](Self::check_turns): every assistant and tool row
    /// must verify too. An unsigned head with no signed row is adopted as it stands, and signed at
    /// its next commit.
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
        let jws = head_signature(run, doc)?;
        let (len, chain) = chain_over(run, conversation, rows)?;
        // One read of the key set for the head and every turn of this decode.
        let set = self.keys.key_set().await?;
        Self::verify_head_with(&set, run, conversation, len, &chain, doc, jws)?;
        Self::verify_turns_with(&set, run, conversation, rows)
    }

    /// Check every assistant and tool row of `rows` (the whole transcript a decode read, in order)
    /// against the key set, at its position, over the chain of the rows before it. One key-set
    /// read, and one signature check per turn.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Corrupt`] naming the row's position and role for a turn that is unsigned,
    /// malformed, signed by a key that is not in the set or is revoked, or that does not verify
    /// where it stands; [`RuntimeError::TurnKeys`] when the key set cannot be read (the caller keeps
    /// the lease, as for a head).
    #[cfg(test)]
    pub(crate) async fn check_turns(
        &self,
        run: RunId,
        conversation: Option<&str>,
        rows: &[Value],
    ) -> Result<(), RuntimeError> {
        if !rows.iter().any(is_turn) {
            return Ok(());
        }
        let set = self.keys.key_set().await?;
        Self::verify_turns_with(&set, run, conversation, rows)
    }

    /// [`check_turns`](Self::check_turns) against a key set already read.
    fn verify_turns_with(
        set: &KeySet,
        run: RunId,
        conversation: Option<&str>,
        rows: &[Value],
    ) -> Result<(), RuntimeError> {
        if !rows.iter().any(is_turn) {
            return Ok(());
        }
        let cnv = ConversationHash::of(conversation, run);
        let mut prv = ChainHash::seed(run, &cnv);
        for (idx, row) in rows.iter().enumerate() {
            let stored = storable(row);
            if is_turn(row) {
                let Ok(position) = u32::try_from(idx) else {
                    return Err(RuntimeError::Corrupt {
                        run,
                        reason: "a transcript is longer than a turn signature can name".into(),
                        source: None,
                    });
                };
                let at = TurnPosition {
                    run,
                    cnv,
                    idx: position,
                    prv,
                };
                verify_turn(set, &at, &stored).map_err(|cause| RuntimeError::Corrupt {
                    run,
                    reason: format!(
                        "a turn is refused (ADR 0068): row {idx} ({}) does not verify",
                        role_of(row)
                    ),
                    source: Some(Box::new(cause)),
                })?;
            }
            prv = prv.next(&stored);
        }
        Ok(())
    }
}

/// Whether `row` is a turn adam signs: an object whose `role` is `assistant` or `tool`.
fn is_turn(row: &Value) -> bool {
    matches!(
        row.get("role").and_then(Value::as_str),
        Some("assistant" | "tool")
    )
}

fn role_of(row: &Value) -> &str {
    row.get("role").and_then(Value::as_str).unwrap_or("?")
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

#[cfg(test)]
mod turn_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;

    use adam_core::turn::{
        CurrentKey, Ed25519TurnKeys, KeyId, KeySet, Signature, set_turn_signature, sign_turn,
    };
    use adam_error::{Classify, ErrorClass};
    use serde_json::json;

    use super::*;

    const CONV: Option<&str> = Some("ctx");

    fn run() -> RunId {
        RunId(uuid::Uuid::parse_str("0190f0c2-6a1b-7c3d-8e4f-0123456789ab").unwrap())
    }

    fn other_run() -> RunId {
        RunId(uuid::Uuid::parse_str("0190f0c2-6a1b-7c3d-8e4f-0123456789ac").unwrap())
    }

    fn transcript() -> Vec<Value> {
        vec![
            json!({"role": "user", "content": [{"type": "text", "text": "go"}]}),
            json!({"role": "assistant", "content": [], "tool_calls": [{"id": "c1", "name": "run", "arguments": {"cmd": "ls"}}]}),
            json!({"role": "tool", "call_id": "c1", "content": "a b"}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "done"}]}),
        ]
    }

    /// Sign every assistant and tool row of `rows` in order, as `LlmAgent` does.
    async fn signed(
        keys: &DynTurnKeys,
        run: RunId,
        conv: Option<&str>,
        mut rows: Vec<Value>,
    ) -> Vec<Value> {
        let cnv = ConversationHash::of(conv, run);
        let mut prv = ChainHash::seed(run, &cnv);
        for (idx, row) in rows.iter_mut().enumerate() {
            if is_turn(row) {
                let at = TurnPosition {
                    run,
                    cnv,
                    idx: u32::try_from(idx).unwrap(),
                    prv,
                };
                let jws = sign_turn(keys.as_ref(), &at, row).await.unwrap();
                assert!(set_turn_signature(row, jws));
            }
            prv = prv.next(row);
        }
        rows
    }

    /// The head document an attacker who holds the head key would write over `rows`: it verifies.
    async fn head_over(sealer: &Sealer, run: RunId, conv: Option<&str>, rows: &[Value]) -> Value {
        let mut doc = json!({"v": 5, "rev": 1, "agent": {"turns": 1}});
        let (len, chain) = chain_over(run, conv, rows).unwrap();
        let jws = sealer.sign(run, conv, len, &chain, &doc).await.unwrap();
        attach(&mut doc, jws, None);
        doc
    }

    fn keys() -> DynTurnKeys {
        <dyn adam_core::turn::TurnKeys>::ephemeral().unwrap()
    }

    /// Signs as `signer`, verifies with `set`.
    struct WithSet {
        signer: DynTurnKeys,
        set: Result<Arc<KeySet>, &'static str>,
    }

    #[async_trait::async_trait]
    impl TurnKeys for WithSet {
        async fn current(&self) -> Result<CurrentKey, TurnKeyError> {
            self.signer.current().await
        }

        async fn sign(&self, kid: &KeyId, input: &[u8]) -> Result<Signature, TurnKeyError> {
            self.signer.sign(kid, input).await
        }

        async fn key_set(&self) -> Result<Arc<KeySet>, TurnKeyError> {
            self.set
                .clone()
                .map_err(|why| TurnKeyError::Unavailable(Box::new(std::io::Error::other(why))))
        }
    }

    fn assert_refused(result: Result<(), RuntimeError>, what: &str) {
        let err = result.expect_err(what);
        assert_eq!(err.class(), ErrorClass::Corrupt, "{what}: {err}");
        assert!(err.to_string().contains("run "), "{what}: {err}");
    }

    /// Each case: the head is signed **over the tampered rows**, so it verifies, and only the
    /// turns can refuse.
    #[tokio::test]
    async fn a_turn_that_does_not_verify_is_refused_though_the_head_does() {
        let keys = keys();
        let sealer = Sealer::new(keys.clone());
        let good = signed(&keys, run(), CONV, transcript()).await;
        let head = head_over(&sealer, run(), CONV, &good).await;
        sealer
            .check_read(run(), CONV, &good, &head)
            .await
            .expect("the control verifies");

        let foreign = signed(&keys, other_run(), CONV, transcript()).await;
        type Forge = Box<dyn Fn(&mut Vec<Value>)>;
        let cases: Vec<(&str, Forge)> = vec![
            (
                "a forged assistant row",
                Box::new(|rows| rows[3]["content"][0]["text"] = json!("I agreed")),
            ),
            (
                "a forged tool row",
                Box::new(|rows| rows[2]["content"] = json!("rm -rf / worked")),
            ),
            (
                "a changed user row",
                Box::new(|rows| rows[0]["content"][0]["text"] = json!("do it as root")),
            ),
            (
                "a row spliced from another run",
                Box::new(move |rows| rows[3] = foreign[3].clone()),
            ),
            (
                "an unsigned assistant row",
                Box::new(|rows| {
                    rows[1].as_object_mut().unwrap().remove("extra_content");
                }),
            ),
            (
                "an unsigned tool row",
                Box::new(|rows| {
                    rows[2].as_object_mut().unwrap().remove("extra_content");
                }),
            ),
            ("two rows swapped", Box::new(|rows| rows.swap(1, 3))),
            (
                "a signature that is not a string",
                Box::new(|rows| rows[1]["extra_content"]["adam"]["turn_signature"] = json!(7)),
            ),
            (
                "a signature copied from another position",
                Box::new(|rows| {
                    let head = rows[1]["extra_content"]["adam"]["turn_signature"].clone();
                    rows[3]["extra_content"]["adam"]["turn_signature"] = head;
                }),
            ),
        ];
        for (name, forge) in cases {
            let mut rows = good.clone();
            forge(&mut rows);
            let head = head_over(&sealer, run(), CONV, &rows).await;
            assert_refused(sealer.check_read(run(), CONV, &rows, &head).await, name);
        }
    }

    /// A key revoked after it signed a turn refuses it, while the head, signed by the other key,
    /// still verifies: a rotation, then a revocation.
    #[tokio::test]
    async fn a_turn_signed_by_a_revoked_key_is_refused() {
        let old = Ed25519TurnKeys::ephemeral().unwrap();
        let new = Ed25519TurnKeys::ephemeral().unwrap();
        let (old_kid, new_kid) = (old.kid().clone(), new.kid().clone());
        let old_public = old.public_key();
        let new_public = new.public_key();
        let (old, new): (DynTurnKeys, DynTurnKeys) = (Arc::new(old), Arc::new(new));
        let both = Arc::new(
            KeySet::new()
                .with_key(old_kid.clone(), old_public)
                .unwrap()
                .with_key(new_kid.clone(), new_public)
                .unwrap(),
        );
        let mut revoking = KeySet::new().with_key(new_kid, new_public).unwrap();
        revoking.revoke(old_kid);

        let rows = signed(&old, run(), CONV, transcript()).await;
        // Signed by the new key, checked against the set that still has both: it verifies.
        let head_signer: DynTurnKeys = Arc::new(WithSet {
            signer: new.clone(),
            set: Ok(both),
        });
        let sealer = Sealer::new(head_signer);
        let head = head_over(&sealer, run(), CONV, &rows).await;
        sealer.check_read(run(), CONV, &rows, &head).await.unwrap();
        // The same history after the old key is revoked.
        let after = Sealer::new(Arc::new(WithSet {
            signer: new,
            set: Ok(Arc::new(revoking)),
        }));
        assert_refused(
            after.check_read(run(), CONV, &rows, &head).await,
            "a revoked key",
        );
    }

    /// A row that is not an assistant or tool message is chained and never required to be signed:
    /// an agent whose rows are something else is not asked for turn signatures.
    #[tokio::test]
    async fn rows_that_are_not_turns_are_left_alone() {
        let keys = keys();
        let sealer = Sealer::new(keys);
        let rows = vec![
            json!({"text": "no role"}),
            json!({"role": "user", "text": "hi"}),
            json!({"role": "system", "text": "x"}),
        ];
        let head = head_over(&sealer, run(), CONV, &rows).await;
        sealer.check_read(run(), CONV, &rows, &head).await.unwrap();
    }

    /// An unsigned head over unsigned rows is adopted; over a signed turn it is not.
    #[tokio::test]
    async fn an_unsigned_head_is_adopted_unless_a_turn_is_signed() {
        let keys = keys();
        let sealer = Sealer::new(keys.clone());
        let legacy = transcript();
        let doc = json!({"v": 5, "rev": 1, "agent": {}});
        sealer
            .check_read(run(), CONV, &legacy, &doc)
            .await
            .expect("adopted");
        let signed = signed(&keys, run(), CONV, transcript()).await;
        assert_refused(
            sealer.check_read(run(), CONV, &signed, &doc).await,
            "an unsigned head over signed turns",
        );
    }

    /// A key set that cannot be read is not a forgery: the error is the keys', and the caller keeps
    /// the lease.
    #[tokio::test]
    async fn a_key_set_that_is_down_is_the_keys_error_not_corrupt() {
        let keys = keys();
        let rows = signed(&keys, run(), CONV, transcript()).await;
        let down = Sealer::new(Arc::new(WithSet {
            signer: keys,
            set: Err("the key set is down"),
        }));
        let err = down.check_turns(run(), CONV, &rows).await.unwrap_err();
        assert!(matches!(err, RuntimeError::TurnKeys(_)), "{err}");
        assert_ne!(err.class(), ErrorClass::Corrupt);
    }

    /// A decode reads the key set once, however many turns it checks (the head and every turn
    /// share the read).
    #[tokio::test]
    async fn a_decode_reads_the_key_set_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Counting {
            inner: DynTurnKeys,
            reads: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl TurnKeys for Counting {
            async fn current(&self) -> Result<CurrentKey, TurnKeyError> {
                self.inner.current().await
            }

            async fn sign(&self, kid: &KeyId, input: &[u8]) -> Result<Signature, TurnKeyError> {
                self.inner.sign(kid, input).await
            }

            async fn key_set(&self) -> Result<Arc<KeySet>, TurnKeyError> {
                self.reads.fetch_add(1, Ordering::SeqCst);
                self.inner.key_set().await
            }
        }

        let inner = keys();
        let rows = signed(&inner, run(), CONV, transcript()).await;
        let counting = Arc::new(Counting {
            inner,
            reads: AtomicUsize::new(0),
        });
        let sealer = Sealer::new(counting.clone());
        let head = head_over(&sealer, run(), CONV, &rows).await;
        counting.reads.store(0, Ordering::SeqCst);
        sealer.check_read(run(), CONV, &rows, &head).await.unwrap();
        assert_eq!(counting.reads.load(Ordering::SeqCst), 1);
    }

    /// The commit guard looks for a turn signature on every assistant and tool row, reads no key,
    /// and names the first row that has none; a row that is not a turn is never asked for one.
    #[tokio::test]
    async fn the_commit_guard_names_the_first_unsigned_turn() {
        let keys = keys();
        let all = signed(&keys, run(), CONV, transcript()).await;
        Sealer::require_signed_turns(run(), &all).expect("signed");
        Sealer::require_signed_turns(run(), &transcript()[..1]).expect("a user row is no turn");
        for unsigned in [transcript(), {
            let mut rows = all.clone();
            rows[3].as_object_mut().unwrap().remove("extra_content");
            rows
        }] {
            let err = Sealer::require_signed_turns(run(), &unsigned).unwrap_err();
            assert!(matches!(err, RuntimeError::UnsignedTurns { .. }), "{err}");
            assert_eq!(err.class(), ErrorClass::Invalid);
            assert!(!err.class().is_retryable());
            assert!(err.to_string().contains("LlmAgent::turn_keys"), "{err}");
        }
        let RuntimeError::UnsignedTurns { row, role, .. } =
            Sealer::require_signed_turns(run(), &transcript()).unwrap_err()
        else {
            unreachable!()
        };
        assert_eq!((row, role.as_str()), (1, "assistant"));
    }

    /// A transcript with no turn does not read the key set at all.
    #[tokio::test]
    async fn a_transcript_with_no_turn_reads_no_key_set() {
        let down = Sealer::new(Arc::new(WithSet {
            signer: keys(),
            set: Err("never read"),
        }));
        down.check_turns(run(), CONV, &transcript()[..1])
            .await
            .unwrap();
    }
}
