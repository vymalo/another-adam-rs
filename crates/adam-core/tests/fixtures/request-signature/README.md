# Request signature test vectors (test only)

`v7-turn-signature.json` is copied byte for byte from the test vectors of the shared prompt-signature contract
(its vector V7, 2026-11-25): two request signatures (`adam-request/v1`) of one model request, V7a by a key named by a
certificate chain (`x5c`), V7b by a key id (`kid`). Ed25519 is deterministic, so the strings are exact.

Every key, seed and certificate in it is **test material only**, made from fixed seeds so that anyone can re-sign; none
is used anywhere else. Do not edit the file: copy it again from the contract's vectors when they change.

`crates/adam-core/tests/request_signature.rs` reproduces both strings with `adam_core::turn::sign_request_at`, verifies
them with `verify_request`, and checks that a changed body, method, path, run or `prompt_jti` fails.
