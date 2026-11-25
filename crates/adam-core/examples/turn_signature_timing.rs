//! Measures the turn signature (ADR 0068 slice T1): one turn signed, one verified, and the chain
//! over a transcript of 1,000 messages. Run it optimised:
//!
//! ```sh
//! cargo run -p adam-core --release --example turn_signature_timing
//! ```

use std::time::Instant;

use adam_core::RunId;
use adam_core::turn::{
    ChainHash, ConversationHash, SignatureType, TurnKeys, TurnPosition, set_turn_signature,
    sign_detached, sign_turn, verify_detached, verify_turn,
};
use serde_json::json;

const ROUNDS: u32 = 2_000;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let keys = <dyn TurnKeys>::ephemeral()?;
    let set = keys.key_set().await?;
    let run = RunId::new();
    let cnv = ConversationHash::of(Some("timing"), run);
    let message = json!({
        "role": "assistant",
        "content": [{"type": "text", "text": "x".repeat(2_000)}],
        "tool_calls": [{"id": "c1", "name": "run", "arguments": {"cmd": "cargo test"}}],
    });
    let position = TurnPosition {
        run,
        cnv,
        idx: 1,
        prv: ChainHash::seed(run, &cnv),
    };

    let raw = b"a 64-byte payload, about the size of a head's chain and length..";
    let started = Instant::now();
    let mut jws = String::new();
    for _ in 0..ROUNDS {
        jws = sign_detached(&*keys, SignatureType::Head, raw).await?;
    }
    let sign_raw = started.elapsed() / ROUNDS;
    let started = Instant::now();
    for _ in 0..ROUNDS {
        verify_detached(&set, SignatureType::Head, raw, &jws)?;
    }
    let verify_raw = started.elapsed() / ROUNDS;

    let started = Instant::now();
    let mut signed = message.clone();
    for _ in 0..ROUNDS {
        signed = message.clone();
        set_turn_signature(&mut signed, sign_turn(&*keys, &position, &message).await?);
    }
    let sign = started.elapsed() / ROUNDS;
    let started = Instant::now();
    for _ in 0..ROUNDS {
        verify_turn(&set, &position, &signed)?;
    }
    let verify = started.elapsed() / ROUNDS;

    let transcript: Vec<_> = (0..1_000).map(|_| signed.clone()).collect();
    let started = Instant::now();
    let chain = ChainHash::over(run, &cnv, &transcript);
    let over = started.elapsed();

    println!("sign, 64-byte payload:   {sign_raw:?}");
    println!("verify, 64-byte payload: {verify_raw:?}");
    println!("sign a 2 KB turn:        {sign:?} (RFC 8785 and the signature)");
    println!("verify a 2 KB turn:      {verify:?}");
    println!("chain over 1,000 turns:  {over:?} ({chain})");
    Ok(())
}
