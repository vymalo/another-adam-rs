//! What a turn signature binds a message to ([ADR 0068] decision 3): its run, its conversation, its
//! position and every message before it.
//!
//! [ADR 0068]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::form::{SignatureType, TurnSignatureError, sign_detached, verify_detached};
use super::{KeyId, KeySet, TurnKeyError, TurnKeys};
use crate::RunId;
use crate::jcs::canonicalize;

/// The `typ` of the chain's first link, `h(-1)`: its own domain, so no other hash of adam's can be
/// taken for it.
const CHAIN_TYP: &str = "adam-chain/v1";

/// `cnv`: the SHA-256 of a run's conversation id (the run's own id when it has none). The
/// conversation is bound without its id being written into every signed payload.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConversationHash([u8; 32]);

impl ConversationHash {
    /// The hash of `conversation`, or of `run`'s id when the run has no conversation.
    pub fn of(conversation: Option<&str>, run: RunId) -> Self {
        let digest = match conversation {
            Some(id) => Sha256::digest(id.as_bytes()),
            None => Sha256::digest(run.to_string().as_bytes()),
        };
        Self(digest.into())
    }

    fn text(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }
}

impl fmt::Display for ConversationHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text())
    }
}

impl fmt::Debug for ConversationHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ConversationHash({})", self.text())
    }
}

/// A link of the chain over a run's transcript:
/// `h(-1) = SHA-256(JCS({typ: "adam-chain/v1", run, cnv}))` ([`seed`](Self::seed)), then
/// `h(i) = SHA-256(h(i-1) ‖ SHA-256(JCS(message i)))` ([`next`](Self::next)), over every message,
/// a person's included, each as stored with its `adam` member. A change to any message, a
/// reordering or a message spliced from another run or position changes every later link.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChainHash([u8; 32]);

impl ChainHash {
    /// `h(-1)`: the chain of a run with no message yet.
    pub fn seed(run: RunId, cnv: &ConversationHash) -> Self {
        let first = json!({"typ": CHAIN_TYP, "run": run.to_string(), "cnv": cnv.text()});
        Self(Sha256::digest(canonicalize(&first)).into())
    }

    /// The chain after `message`.
    #[must_use]
    pub fn next(&self, message: &Value) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(self.0);
        hasher.update(Sha256::digest(canonicalize(message)));
        Self(hasher.finalize().into())
    }

    /// The chain over `messages`, from the seed: `h(len - 1)`.
    pub fn over<'a>(
        run: RunId,
        cnv: &ConversationHash,
        messages: impl IntoIterator<Item = &'a Value>,
    ) -> Self {
        messages
            .into_iter()
            .fold(Self::seed(run, cnv), |chain, message| chain.next(message))
    }

    /// The link's 32 bytes.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0
    }

    /// The link written by [`Display`](fmt::Display) (base64url, no padding): `None` for anything
    /// that is not exactly 32 bytes of it.
    pub fn parse(text: &str) -> Option<Self> {
        let bytes = URL_SAFE_NO_PAD.decode(text).ok()?;
        <[u8; 32]>::try_from(bytes).ok().map(Self)
    }

    fn text(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }
}

impl fmt::Display for ChainHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text())
    }
}

impl fmt::Debug for ChainHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChainHash({})", self.text())
    }
}

/// Where a turn is: the run that commits it, its conversation, its index in the transcript and the
/// chain of every message before it (`prv = h(idx - 1)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TurnPosition {
    /// The run that commits the turn.
    pub run: RunId,
    /// The run's conversation.
    pub cnv: ConversationHash,
    /// The turn's index in the transcript.
    pub idx: u32,
    /// The chain of the messages before it.
    pub prv: ChainHash,
}

/// The bytes a turn signature covers: the RFC 8785 form of `{typ, run, cnv, idx, prv, msg}`, where
/// `msg` is `message` without its `extra_content.adam` member (and without `extra_content` when that
/// was its only member). A provider's members are part of `msg`, so they are signed too.
pub fn turn_payload(position: &TurnPosition, message: &Value) -> Vec<u8> {
    canonicalize(&json!({
        "typ": SignatureType::Turn.typ(),
        "run": position.run.to_string(),
        "cnv": position.cnv.text(),
        "idx": position.idx,
        "prv": position.prv.text(),
        "msg": without_adam(message),
    }))
}

fn without_adam(message: &Value) -> Value {
    let mut message = message.clone();
    if let Some(map) = message.as_object_mut()
        && let Some(Value::Object(extra)) = map.get_mut("extra_content")
    {
        extra.remove("adam");
        if extra.is_empty() {
            map.remove("extra_content");
        }
    }
    message
}

/// The bytes a head signature covers ([ADR 0068] decision 4): the RFC 8785 form of
/// `{typ: "adam-head/v1", run, cnv, len, chain, head}`. `len` is the number of body rows, `chain` is
/// `h(len - 1)` over them ([`ChainHash::over`]) and `head` is the run's state document **as the
/// runtime wrote it, without its signature** (the caller decides which members that is: see the
/// runtime's `seal` module). A changed head member, a row appended or dropped (`len`, `chain`), and
/// a run moved to another run id or conversation (`run`, `cnv`) each change these bytes.
///
/// [ADR 0068]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md
pub fn head_payload(
    run: RunId,
    cnv: &ConversationHash,
    len: u32,
    chain: &ChainHash,
    head: &Value,
) -> Vec<u8> {
    canonicalize(&json!({
        "typ": SignatureType::Head.typ(),
        "run": run.to_string(),
        "cnv": cnv.text(),
        "len": len,
        "chain": chain.text(),
        "head": head,
    }))
}

/// Sign a run's head with the current key of `keys`: the detached JWS of [`head_payload`].
///
/// # Errors
///
/// [`TurnKeyError`] from the keys.
pub async fn sign_head(
    keys: &(impl TurnKeys + ?Sized),
    run: RunId,
    cnv: &ConversationHash,
    len: u32,
    chain: &ChainHash,
    head: &Value,
) -> Result<String, TurnKeyError> {
    sign_detached(
        keys,
        SignatureType::Head,
        &head_payload(run, cnv, len, chain, head),
    )
    .await
}

/// Verify `jws` as the head signature of this run, conversation, length, chain and head, with
/// `set`; the id of the key that made it.
///
/// # Errors
///
/// [`TurnSignatureError`], all of class `Corrupt`; a turn signature offered as a head's is
/// [`WrongType`](TurnSignatureError::WrongType).
pub fn verify_head(
    set: &KeySet,
    run: RunId,
    cnv: &ConversationHash,
    len: u32,
    chain: &ChainHash,
    head: &Value,
    jws: &str,
) -> Result<KeyId, TurnSignatureError> {
    verify_detached(
        set,
        SignatureType::Head,
        &head_payload(run, cnv, len, chain, head),
        jws,
    )
}

/// Sign `message` at `position` with the current key of `keys`: the value of its
/// `extra_content.adam.turn_signature` ([`set_turn_signature`]).
///
/// # Errors
///
/// [`TurnKeyError`] from the keys.
pub async fn sign_turn(
    keys: &(impl TurnKeys + ?Sized),
    position: &TurnPosition,
    message: &Value,
) -> Result<String, TurnKeyError> {
    sign_detached(keys, SignatureType::Turn, &turn_payload(position, message)).await
}

/// Verify the turn signature `message` carries, at `position`, with `set`; the id of the key that
/// made it.
///
/// # Errors
///
/// [`TurnSignatureError::Unsigned`] when it carries none, [`TurnSignatureError::Malformed`] when
/// the member is not a string, and the others of [`verify_detached`].
pub fn verify_turn(
    set: &KeySet,
    position: &TurnPosition,
    message: &Value,
) -> Result<KeyId, TurnSignatureError> {
    let member = message
        .get("extra_content")
        .and_then(|extra| extra.get("adam"))
        .and_then(|adam| adam.get("turn_signature"))
        .ok_or(TurnSignatureError::Unsigned)?;
    let jws = member.as_str().ok_or(TurnSignatureError::Malformed)?;
    verify_detached(
        set,
        SignatureType::Turn,
        &turn_payload(position, message),
        jws,
    )
}

/// The turn signature `message` carries, when it carries one as a string.
pub fn turn_signature(message: &Value) -> Option<&str> {
    message
        .get("extra_content")?
        .get("adam")?
        .get("turn_signature")?
        .as_str()
}

/// Put `signature` in `message` as `extra_content.adam.turn_signature`, beside a provider's members.
/// `false`, and nothing changed, when `message` is not an object or its `extra_content` or `adam`
/// member is not one.
pub fn set_turn_signature(message: &mut Value, signature: String) -> bool {
    let Some(map) = message.as_object_mut() else {
        return false;
    };
    let extra = map
        .entry("extra_content")
        .or_insert_with(|| Value::Object(Default::default()));
    let Some(extra) = extra.as_object_mut() else {
        return false;
    };
    let adam = extra
        .entry("adam")
        .or_insert_with(|| Value::Object(Default::default()));
    let Some(adam) = adam.as_object_mut() else {
        return false;
    };
    adam.insert("turn_signature".into(), Value::String(signature));
    true
}
