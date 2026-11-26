#!/usr/bin/env python3
"""Regenerate the golden vector of `src/signing.rs` (ADR 0068, slice T3a-1).

Independent of the Rust code: standard library plus `cryptography` (`pip install cryptography`).
The key is the RFC 8032 section 7.1 test 1 key, public and never a real one. Prints the Rust
constants; run `python3 turn_chain_vector.py` and compare with `GOLDEN_*` in `signing.rs`.

RFC 8785 (JCS) is `json.dumps(sort_keys=True, separators=(",", ":"), ensure_ascii=False)` for
these values (no floats beyond 1.5, no escapes that differ).
"""
import base64
import hashlib
import json
import uuid

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

SECRET = bytes.fromhex("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60")
KID = "vault:turn-key#v1"
RUN = str(uuid.UUID("0190f0c2-6a1b-7c3d-8e4f-0123456789ab"))
CONVERSATION = "ctx-1"


def jcs(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()


def b64(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


CNV = b64(hashlib.sha256(CONVERSATION.encode()).digest())


def chain(prv, message):
    """h(i) = SHA-256(h(i-1) || SHA-256(JCS(message))), over the message with its signature."""
    return hashlib.sha256(prv + hashlib.sha256(jcs(message)).digest()).digest()


def without_adam(message):
    message = json.loads(json.dumps(message))
    extra = message.get("extra_content")
    if isinstance(extra, dict):
        extra.pop("adam", None)
        if not extra:
            del message["extra_content"]
    return message


def sign(idx, prv, message):
    payload = jcs({"typ": "adam-turn/v1", "run": RUN, "cnv": CNV, "idx": idx,
                   "prv": b64(prv), "msg": without_adam(message)})
    protected = b64(jcs({"alg": "EdDSA", "kid": KID, "typ": "adam-turn/v1"}))
    signing_input = (protected + "." + b64(payload)).encode()
    signature = Ed25519PrivateKey.from_private_bytes(SECRET).sign(signing_input)
    return protected + ".." + b64(signature)


def signed(idx, prv, message):
    message = dict(message)
    message["extra_content"] = {"adam": {"turn_signature": sign(idx, prv, message)}}
    return message


messages = [
    {"role": "user", "content": [{"type": "text", "text": "Bonjour, ça va ? 😀"}]},
    {"role": "assistant", "content": [],
     "tool_calls": [{"id": "c1", "name": "run", "arguments": {"cmd": "cargo test", "n": 1.5}}]},
    {"role": "tool", "call_id": "c1", "content": "12 passed", "is_error": False},
]

prv = hashlib.sha256(jcs({"typ": "adam-chain/v1", "run": RUN, "cnv": CNV})).digest()
out = []
prvs = []
for idx, message in enumerate(messages):
    prvs.append(prv)
    if message["role"] != "user":
        message = signed(idx, prv, message)
    out.append(message)
    prv = chain(prv, message)

print(f'const GOLDEN_PRV_ASSISTANT: &str = "{b64(prvs[1])}";')
print(f'const GOLDEN_PRV_TOOL: &str = "{b64(prvs[2])}";')
print(f'const GOLDEN_HEAD: &str = "{b64(prv)}";')
print(f'const GOLDEN_ASSISTANT_JWS: &str = "{out[1]["extra_content"]["adam"]["turn_signature"]}";')
print(f'const GOLDEN_TOOL_JWS: &str = "{out[2]["extra_content"]["adam"]["turn_signature"]}";')
