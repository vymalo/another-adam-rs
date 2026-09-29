//! What goes into a `NOTIFY` payload, and the size rule.
//!
//! PostgreSQL rejects a payload of 8000 bytes or more, and a rejected `NOTIFY`
//! would fail the statement, so nothing bigger than [`MAX_PAYLOAD_BYTES`] is
//! ever sent: a `Status` event loses the tail of its detail, and anything else
//! that does not fit is dropped.

use adam_core::RunId;
use adam_runtime::{RunEvent, Signal};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The largest payload sent: PostgreSQL requires it to be shorter than 8000
/// bytes (*verified* 2026-09-29, PostgreSQL 16 docs, `NOTIFY`).
pub const MAX_PAYLOAD_BYTES: usize = 7_999;

/// Version of both payload shapes. A payload of another version is ignored.
pub(crate) const WIRE_VERSION: u8 = 1;

/// One run event on the `events` channel.
#[derive(Debug, Deserialize)]
pub(crate) struct EventIn {
    pub v: u8,
    /// The `PgNotify` that sent it, so it can skip its own.
    pub o: Uuid,
    pub run: RunId,
    pub agent: String,
    pub event: RunEvent,
}

#[derive(Serialize)]
struct EventOut<'a> {
    v: u8,
    o: Uuid,
    run: RunId,
    agent: &'a str,
    event: &'a RunEvent,
}

#[derive(Deserialize)]
pub(crate) struct SignalIn {
    pub v: u8,
    #[serde(flatten)]
    pub signal: Signal,
}

#[derive(Serialize)]
struct SignalOut<'a> {
    v: u8,
    #[serde(flatten)]
    signal: &'a Signal,
}

/// The outcome of fitting an event into a payload.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Encoded {
    /// As is.
    Fits(String),
    /// A `Status` whose detail was cut to fit.
    Truncated(String),
    /// Does not fit and cannot be made to.
    Dropped,
}

fn event_json(origin: Uuid, run: RunId, agent: &str, event: &RunEvent) -> Option<String> {
    serde_json::to_string(&EventOut {
        v: WIRE_VERSION,
        o: origin,
        run,
        agent,
        event,
    })
    .ok()
}

pub(crate) fn encode_event(origin: Uuid, run: RunId, agent: &str, event: &RunEvent) -> Encoded {
    let Some(full) = event_json(origin, run, agent, event) else {
        return Encoded::Dropped;
    };
    if full.len() <= MAX_PAYLOAD_BYTES {
        return Encoded::Fits(full);
    }
    // Only a status crosses when too big: it is what wakes a subscriber to
    // re-read the durable record, and the detail is a courtesy (the full
    // failure text is in the run itself). Progress, custom events and
    // artifacts are dropped; artifacts still reach subscribers through the
    // durable poll.
    let RunEvent::Status {
        status,
        detail: Some(detail),
    } = event
    else {
        return Encoded::Dropped;
    };
    // The largest prefix (on a character boundary) that fits. The payload only
    // grows with the prefix, so a binary search finds it.
    let with = |keep: usize| {
        let cut = RunEvent::Status {
            status: *status,
            detail: (keep > 0).then(|| detail[..keep].to_owned()),
        };
        event_json(origin, run, agent, &cut).filter(|json| json.len() <= MAX_PAYLOAD_BYTES)
    };
    let Some(mut best) = with(0) else {
        // The run and agent alone do not fit.
        return Encoded::Dropped;
    };
    // Prefix lengths that end on a character boundary; index 0 is the empty
    // prefix, which fits.
    let bounds: Vec<usize> = detail
        .char_indices()
        .map(|(i, _)| i)
        .chain([detail.len()])
        .collect();
    let (mut lo, mut hi) = (0, bounds.len() - 1);
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        match with(bounds[mid]) {
            Some(json) => {
                best = json;
                lo = mid;
            }
            None => hi = mid - 1,
        }
    }
    Encoded::Truncated(best)
}

/// `None` when the signal does not fit (an absurdly long agent name).
pub(crate) fn encode_signal(signal: &Signal) -> Option<String> {
    let json = serde_json::to_string(&SignalOut {
        v: WIRE_VERSION,
        signal,
    })
    .ok()?;
    (json.len() <= MAX_PAYLOAD_BYTES).then_some(json)
}

#[cfg(test)]
mod tests {
    use adam_core::RunStatus;
    use serde_json::json;

    use super::*;

    fn decode(json: &str) -> EventIn {
        serde_json::from_str(json).expect("decodes")
    }

    #[test]
    fn events_roundtrip_with_their_origin() {
        let origin = Uuid::new_v4();
        let run = RunId::new();
        let events = [
            RunEvent::Status {
                status: RunStatus::Parked,
                detail: Some("waiting".into()),
            },
            RunEvent::Progress {
                message: "hi".into(),
            },
            RunEvent::Custom {
                kind: "k".into(),
                payload: json!({"a": [1, 2]}),
            },
            RunEvent::Artifact {
                name: "n".into(),
                mime_type: None,
                data: json!("x"),
            },
        ];
        for event in events {
            let Encoded::Fits(json) = encode_event(origin, run, "agent", &event) else {
                panic!("small events fit");
            };
            let back = decode(&json);
            assert_eq!(back.v, WIRE_VERSION);
            assert_eq!(
                (back.o, back.run, back.agent.as_str()),
                (origin, run, "agent")
            );
            assert_eq!(back.event, event);
        }
    }

    #[test]
    fn the_payload_layout_is_stable() {
        let origin = Uuid::nil();
        let run = RunId::new();
        let Encoded::Fits(json) = encode_event(
            origin,
            run,
            "a",
            &RunEvent::Progress {
                message: "m".into(),
            },
        ) else {
            panic!("fits");
        };
        let v: serde_json::Value = serde_json::from_str(&json).expect("json");
        assert_eq!(
            v,
            json!({
                "v": 1,
                "o": "00000000-0000-0000-0000-000000000000",
                "run": run.to_string(),
                "agent": "a",
                "event": {"type": "progress", "message": "m"},
            })
        );
    }

    #[test]
    fn signals_roundtrip() {
        for signal in [
            Signal::Runnable {
                run: RunId::new(),
                agent: "coder".into(),
            },
            Signal::Finished { run: RunId::new() },
        ] {
            let json = encode_signal(&signal).expect("fits");
            let v: serde_json::Value = serde_json::from_str(&json).expect("json");
            assert_eq!(v["v"], 1);
            assert!(v["type"].is_string());
            let back: SignalIn = serde_json::from_str(&json).expect("decodes");
            assert_eq!((back.v, back.signal), (WIRE_VERSION, signal));
        }
    }

    #[test]
    fn a_payload_of_exactly_the_limit_fits_and_one_more_byte_does_not() {
        let (origin, run) = (Uuid::new_v4(), RunId::new());
        let overhead = match encode_event(
            origin,
            run,
            "a",
            &RunEvent::Progress {
                message: String::new(),
            },
        ) {
            Encoded::Fits(s) => s.len(),
            other => panic!("{other:?}"),
        };
        let at = |n: usize| RunEvent::Progress {
            message: "x".repeat(n),
        };
        let room = MAX_PAYLOAD_BYTES - overhead;
        match encode_event(origin, run, "a", &at(room)) {
            Encoded::Fits(s) => assert_eq!(s.len(), MAX_PAYLOAD_BYTES),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            encode_event(origin, run, "a", &at(room + 1)),
            Encoded::Dropped
        );
    }

    #[test]
    fn an_oversize_status_loses_the_tail_of_its_detail() {
        let (origin, run) = (Uuid::new_v4(), RunId::new());
        let detail = "d".repeat(20_000);
        let event = RunEvent::Status {
            status: RunStatus::Failed,
            detail: Some(detail.clone()),
        };
        let Encoded::Truncated(json) = encode_event(origin, run, "agent", &event) else {
            panic!("a status is truncated, not dropped");
        };
        assert!(json.len() <= MAX_PAYLOAD_BYTES);
        let RunEvent::Status {
            status,
            detail: Some(cut),
        } = decode(&json).event
        else {
            panic!("still a status with a detail");
        };
        assert_eq!(status, RunStatus::Failed);
        assert!(detail.starts_with(&cut) && cut.len() < detail.len());
        // Cut as little as needed.
        assert_eq!(json.len(), MAX_PAYLOAD_BYTES);
    }

    #[test]
    fn truncation_never_splits_a_multibyte_character_or_overshoots_on_escapes() {
        let (origin, run) = (Uuid::new_v4(), RunId::new());
        // Three-byte characters, four-byte characters, and characters JSON
        // escapes (quotes, backslashes, control characters).
        for unit in ["é", "€", "𝄞", "\"", "\\", "\n", "\u{1}"] {
            let detail = unit.repeat(9_000 / unit.len().max(1) + 100);
            let event = RunEvent::Status {
                status: RunStatus::Done,
                detail: Some(detail.clone()),
            };
            let encoded = encode_event(origin, run, "agent", &event);
            let Encoded::Truncated(json) = encoded else {
                panic!("{unit:?}: {encoded:?}");
            };
            assert!(json.len() <= MAX_PAYLOAD_BYTES, "{unit:?}: {}", json.len());
            let RunEvent::Status {
                detail: Some(cut), ..
            } = decode(&json).event
            else {
                panic!("{unit:?}: detail survives");
            };
            assert!(detail.starts_with(&cut), "{unit:?}: a prefix");
            assert!(
                cut.chars().all(|c| unit.starts_with(c)),
                "{unit:?}: whole characters"
            );
            // As long as it can be: the next character would not fit.
            let next = detail[cut.len()..].chars().next().expect("more detail");
            let longer = RunEvent::Status {
                status: RunStatus::Done,
                detail: Some(format!("{cut}{next}")),
            };
            assert!(
                encode_event(origin, run, "agent", &longer) == Encoded::Truncated(json.clone()),
                "{unit:?}: one more character does not fit, so it is cut again to the same"
            );
        }
    }

    #[test]
    fn other_oversize_events_are_dropped() {
        let (origin, run) = (Uuid::new_v4(), RunId::new());
        let big = "x".repeat(10_000);
        for event in [
            RunEvent::Progress {
                message: big.clone(),
            },
            RunEvent::Custom {
                kind: "k".into(),
                payload: json!(big.clone()),
            },
            RunEvent::Artifact {
                name: "n".into(),
                mime_type: None,
                data: json!(big.clone()),
            },
            RunEvent::Status {
                status: RunStatus::Done,
                detail: None,
            }, // small, control: fits
        ] {
            let small = matches!(event, RunEvent::Status { .. });
            let encoded = encode_event(origin, run, "agent", &event);
            assert_eq!(matches!(encoded, Encoded::Fits(_)), small, "{event:?}");
            assert!(small || encoded == Encoded::Dropped, "{event:?}");
        }
    }

    #[test]
    fn a_status_whose_agent_name_alone_is_too_long_is_dropped() {
        let event = RunEvent::Status {
            status: RunStatus::Done,
            detail: Some("x".into()),
        };
        let agent = "a".repeat(9_000);
        assert_eq!(
            encode_event(Uuid::new_v4(), RunId::new(), &agent, &event),
            Encoded::Dropped
        );
        let signal = Signal::Runnable {
            run: RunId::new(),
            agent,
        };
        assert_eq!(encode_signal(&signal), None);
    }

    #[test]
    fn a_payload_of_another_version_or_shape_does_not_decode_as_ours() {
        assert!(serde_json::from_str::<EventIn>("not json").is_err());
        assert!(serde_json::from_str::<EventIn>(r#"{"v":2,"x":1}"#).is_err());
        assert!(serde_json::from_str::<SignalIn>(r#"{"v":1,"type":"teleported"}"#).is_err());
    }
}
