//! What goes into a `NOTIFY` payload, and the size rule.
//!
//! PostgreSQL rejects a payload of 8000 bytes or more, and a rejected `NOTIFY`
//! would fail the statement, so nothing bigger than [`MAX_PAYLOAD_BYTES`] is
//! ever sent: a `Status` event loses the tail of its detail, a `Step` loses its
//! input and output, and anything else that does not fit is dropped (a file artifact
//! almost always: it reaches subscribers through the durable record).

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
    /// A `Status` whose detail was cut to fit, or a `Step` that was sent without its input and
    /// output.
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
    // A file that cannot fit a payload (its base64 is a third more than its bytes) is not
    // serialized to find out: it reaches subscribers through the durable record, like any
    // artifact that does not fit.
    if let RunEvent::Artifact {
        file: Some(file), ..
    } = event
        && file.bytes.len() > MAX_PAYLOAD_BYTES
    {
        return Encoded::Dropped;
    }
    let Some(full) = event_json(origin, run, agent, event) else {
        return Encoded::Dropped;
    };
    if full.len() <= MAX_PAYLOAD_BYTES {
        return Encoded::Fits(full);
    }
    // A step crosses without its input and output (up to 4 KiB and 8 KiB, more than a payload
    // holds beside the rest of the step): they are a courtesy, and the step, its state and its
    // label are what a subscriber needs. An end that lost its output is still an end.
    if let RunEvent::Step(step) = event
        && (step.input.is_some() || step.output.is_some())
    {
        let mut bare = step.clone();
        bare.input = None;
        bare.output = None;
        return match event_json(origin, run, agent, &RunEvent::Step(bare))
            .filter(|json| json.len() <= MAX_PAYLOAD_BYTES)
        {
            Some(json) => Encoded::Truncated(json),
            None => Encoded::Dropped,
        };
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
    use adam_runtime::{
        MAX_STEP_DETAIL_CHARS, MAX_STEP_ID_BYTES, MAX_STEP_LABEL_CHARS, MAX_STREAM_ID_BYTES,
        MAX_TEXT_DELTA_BYTES, StepEvent, StepIcon, StepKind, StepState,
    };
    use serde_json::json;

    use super::*;

    /// A file small enough for a payload.
    fn small_file() -> adam_runtime::ArtifactFile {
        let artifact = adam_runtime::Artifact::file(
            "logo",
            "image/svg+xml",
            "logo.svg",
            b"<svg xmlns='http://www.w3.org/2000/svg'/>".to_vec(),
        )
        .expect("a small file");
        artifact.file.expect("a file")
    }

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
                file: None,
            },
            RunEvent::Artifact {
                name: "logo".into(),
                mime_type: Some("image/svg+xml".into()),
                data: serde_json::Value::Null,
                file: Some(small_file()),
            },
            RunEvent::Step(
                StepEvent::new("tool:c1", StepKind::Tool, "run_checks", StepState::Running)
                    .with_detail("running checks"),
            ),
            RunEvent::TextDelta {
                stream: "run-m0-a1b2c3d4".into(),
                offset: 3,
                text: "onacci ".into(),
                last: false,
                abandoned: false,
            },
            RunEvent::ReasoningDelta {
                stream: "run-r0-a1b2c3d4".into(),
                offset: 0,
                text: "The user asks".into(),
                last: false,
                abandoned: false,
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

    /// A step is bounded by its constructors (an id, a label, a detail), so the largest one a tool can make
    /// crosses the channel whole, even in the characters that take four bytes.
    #[test]
    fn the_largest_step_event_fits_a_payload() {
        let step = StepEvent::new(
            "i".repeat(MAX_STEP_ID_BYTES),
            StepKind::Subagent,
            "🙂".repeat(MAX_STEP_LABEL_CHARS),
            StepState::Failed,
        )
        .under("p".repeat(MAX_STEP_ID_BYTES))
        .with_icon(StepIcon::Execute)
        .with_detail("🙂".repeat(MAX_STEP_DETAIL_CHARS));
        // Four bytes a character, escaped as nothing (JSON keeps them): about 5 KiB.
        let Encoded::Fits(json) = encode_event(
            Uuid::new_v4(),
            RunId::new(),
            &"a".repeat(64),
            &RunEvent::Step(step.clone()),
        ) else {
            panic!("the largest step fits");
        };
        assert!(json.len() < MAX_PAYLOAD_BYTES, "{} bytes", json.len());
        assert_eq!(decode(&json).event, RunEvent::Step(step));
    }

    /// A step with the most input or output its contract allows does not fit a payload beside the rest of the
    /// step: it crosses without them, and is the same step otherwise. A step whose input and output fit
    /// crosses whole.
    #[test]
    fn a_step_that_is_too_big_crosses_without_its_input_and_output() {
        use adam_runtime::{STEP_OUTPUT_MAX_BYTES, StepOutput};

        let origin = Uuid::new_v4();
        let run = RunId::new();
        let ended = StepEvent::new("tool:c1", StepKind::Tool, "Search", StepState::Completed);
        let big_output = ended
            .clone()
            .with_output(StepOutput::new("m\"n".repeat(STEP_OUTPUT_MAX_BYTES), false));
        let Encoded::Truncated(json) = encode_event(origin, run, "a", &RunEvent::Step(big_output))
        else {
            panic!("an output of 8 KiB of quotes does not fit a payload");
        };
        assert!(json.len() <= MAX_PAYLOAD_BYTES);
        assert_eq!(decode(&json).event, RunEvent::Step(ended.clone()));

        // The most input the contract allows, beside a step that is itself large (a detail of four-byte
        // characters): over a payload.
        let started = StepEvent::new("tool:c1", StepKind::Tool, "Search", StepState::Running)
            .with_detail("🙂".repeat(MAX_STEP_DETAIL_CHARS));
        let input =
            serde_json::Map::from_iter((0..8).map(|n| (format!("k{n}"), json!("a".repeat(500)))));
        let big_input = started.clone().with_input(input);
        assert!(
            big_input
                .input
                .as_ref()
                .is_some_and(|i| !i.contains_key("_cut"))
        );
        let Encoded::Truncated(json) = encode_event(origin, run, "a", &RunEvent::Step(big_input))
        else {
            panic!("a step with 4 KiB of input and a large detail does not fit a payload");
        };
        assert_eq!(decode(&json).event, RunEvent::Step(started));

        // Small ones cross whole.
        let small = ended.with_output(StepOutput::new("1. Example Domain", false));
        let Encoded::Fits(json) = encode_event(origin, run, "a", &RunEvent::Step(small.clone()))
        else {
            panic!("a small output fits");
        };
        assert_eq!(decode(&json).event, RunEvent::Step(small));
    }

    /// A piece of streamed text is bounded by `MAX_TEXT_DELTA_BYTES` (what `adam-llm-agent` cuts at), so the
    /// largest one crosses the channel whole: even a piece of control characters, which JSON writes in six
    /// bytes each.
    #[test]
    fn the_largest_text_delta_fits_a_payload() {
        for character in ['\u{0}', '\u{1f}', '"', '\\', 'a'] {
            let delta = RunEvent::TextDelta {
                stream: "s".repeat(MAX_STREAM_ID_BYTES),
                offset: u64::MAX,
                text: character.to_string().repeat(MAX_TEXT_DELTA_BYTES),
                last: true,
                abandoned: true,
            };
            let Encoded::Fits(json) =
                encode_event(Uuid::new_v4(), RunId::new(), &"a".repeat(64), &delta)
            else {
                panic!("the largest text delta of {character:?} fits");
            };
            assert!(json.len() < MAX_PAYLOAD_BYTES, "{} bytes", json.len());
            assert_eq!(decode(&json).event, delta);
        }
        // The bound is on bytes: 256 four-byte characters are as many as a thousand and twenty-four.
        let wide = RunEvent::TextDelta {
            stream: "s".into(),
            offset: 0,
            text: "🙂".repeat(MAX_TEXT_DELTA_BYTES / 4),
            last: false,
            abandoned: false,
        };
        assert!(matches!(
            encode_event(Uuid::new_v4(), RunId::new(), "a", &wide),
            Encoded::Fits(_)
        ));
    }

    /// Reasoning has a text delta's bounds, so its largest piece crosses whole too; and a process that
    /// predates the variant cannot read it (it is not text: it is dropped, never shown as the answer).
    #[test]
    fn the_largest_reasoning_delta_fits_a_payload_and_is_not_a_text_delta() {
        for character in ['\u{0}', '\u{1f}', '"', 'a'] {
            let delta = RunEvent::ReasoningDelta {
                stream: "s".repeat(MAX_STREAM_ID_BYTES),
                offset: u64::MAX,
                text: character.to_string().repeat(MAX_TEXT_DELTA_BYTES),
                last: true,
                abandoned: true,
            };
            let Encoded::Fits(json) =
                encode_event(Uuid::new_v4(), RunId::new(), &"a".repeat(64), &delta)
            else {
                panic!("the largest reasoning delta of {character:?} fits");
            };
            assert!(json.len() < MAX_PAYLOAD_BYTES, "{} bytes", json.len());
            assert_eq!(decode(&json).event, delta);
            assert!(json.contains("\"reasoning_delta\""), "{json}");
            assert!(!json.contains("\"text_delta\""), "{json}");
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
                file: None,
            },
            // A file whose bytes alone are over a payload, dropped without being serialized.
            RunEvent::from(
                adam_runtime::Artifact::file("f", "image/png", "f.png", vec![7; 10_000])
                    .expect("a file within the cap"),
            ),
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
