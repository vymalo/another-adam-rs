# Golden vectors

`turn_chain_vector.py` regenerates the constants of the golden test in `src/signing.rs` (ADR 0068, slice T3a-1) with
no Rust: standard library plus `cryptography` (`pip install cryptography`), the RFC 8032 section 7.1 test 1 key (public,
never a real one). `python3 turn_chain_vector.py` prints the `GOLDEN_*` constants; they must equal the ones in the test,
byte for byte (checked 2026-11-26).
