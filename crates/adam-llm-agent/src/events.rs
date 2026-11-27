//! Custom events of a tool call (ADR 0050, slice **H5**).
//!
//! A tool declares the events it emits ([`Tool::events`](crate::Tool::events)), the agent may declare
//! more ([`LlmAgentBuilder::event`](crate::LlmAgentBuilder::event)), and a call emits one with
//! [`ToolCtx::emit_event`](crate::ToolCtx::emit_event). The emit is a **record**, not a send: the
//! event is held with the call and journaled in the call's step ([`HookedOutput::events`]), so what
//! reaches observers and clients is read from the record, after the step, in call order. That is why
//! a replay that finds the step recorded delivers the event again with the same key
//! (`<run>/tool:<call id>/<name>/<index>`) and the tool does not run twice.
//!
//! The bounds are checked at the emit, where the tool can be told: an undeclared name, data over
//! [`DATA_MAX`], a seventeenth event in one step and a 257th in one transition are refused with an
//! [`EmitError`] and not recorded.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use adam_error::{Classify, ErrorClass};
use adam_plugin::{
    DATA_MAX, DeclarationError, EventSpec, STEP_EVENTS_MAX, TRANSITION_EVENTS_MAX, declared,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tool::DynTool;

/// A custom event as the step that emitted it records it.
///
/// The record holds the name and the version, not the visibility: who may see an event is the
/// declaration of the build that delivers it, so a deployment that narrows an event narrows it for a
/// replay too. A name the build no longer declares is not delivered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordedEvent {
    /// The declared name, `ns.name`.
    pub name: String,
    /// The version of the data, as declared when it was emitted.
    pub version: u32,
    /// The data, scrubbed, at most [`DATA_MAX`] bytes serialized.
    pub data: Value,
}

/// Why [`ToolCtx::emit_event`](crate::ToolCtx::emit_event) refused an event. Nothing was recorded,
/// and nothing will be sent. The class is [`ErrorClass::Invalid`]: the tool is wrong, and trying
/// again changes nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EmitError {
    /// The tool did not declare this event (or the agent did not): see
    /// [`Tool::events`](crate::Tool::events).
    #[error("the event `{0}` is not declared by this tool or by the agent")]
    Undeclared(String),
    /// The data serializes to more than [`DATA_MAX`] bytes.
    #[error("the data of the event `{name}` is {size} bytes: the most is {DATA_MAX}")]
    TooLarge {
        /// The event.
        name: String,
        /// Its serialized size.
        size: usize,
    },
    /// The call already emitted [`STEP_EVENTS_MAX`] events.
    #[error("a step emits at most {STEP_EVENTS_MAX} events")]
    TooManyInStep,
    /// The transition already emitted [`TRANSITION_EVENTS_MAX`] events, across all its calls.
    #[error("a transition emits at most {TRANSITION_EVENTS_MAX} events")]
    TooManyInTransition,
    /// This context belongs to no run's call (a detached one declares no event).
    #[error("this context declares no events")]
    Unavailable,
}

impl Classify for EmitError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}

/// How many events the calls of **one transition** have emitted: one counter for all of them, made
/// when the conversation is loaded for the transition and shared by every call's context. A counter
/// per call would allow 16 times the calls.
///
/// Not part of the conversation's record: it is skipped when serialized and is new on every load, and
/// two conversations are equal whatever their counters say. Public only because a field of
/// [`Conversation`](crate::Conversation) is; it has no use outside the crate.
#[derive(Debug, Clone, Default)]
pub struct TransitionBudget(pub(crate) Arc<AtomicUsize>);

impl PartialEq for TransitionBudget {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

/// What one call may emit, and where its events are held until the step records them.
#[derive(Debug, Clone)]
pub(crate) struct EventScope {
    /// The declarations this call may emit: its tool's own and the agent's.
    allowed: Arc<BTreeMap<String, EventSpec>>,
    /// The transition's counter, shared by every call (and every clone of the context).
    transition: Arc<AtomicUsize>,
    /// What the call has emitted so far, shared by the clones of its context.
    held: Arc<Mutex<Vec<RecordedEvent>>>,
}

impl EventScope {
    pub(crate) fn new(
        allowed: Arc<BTreeMap<String, EventSpec>>,
        transition: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            allowed,
            transition,
            held: Arc::default(),
        }
    }

    /// Records `data` as the event `name`, within the declarations and the bounds.
    pub(crate) fn emit(
        &self,
        name: &str,
        mut data: Value,
        scrub: &dyn Fn(&str) -> String,
    ) -> Result<(), EmitError> {
        let spec = self
            .allowed
            .get(name)
            .ok_or_else(|| EmitError::Undeclared(name.to_owned()))?;
        // The tool's data is durable and goes to observers and clients: a secret in it is scrubbed
        // here, before the size is judged, as the loop scrubs what it records elsewhere.
        scrub_strings(&mut data, scrub);
        let size = data.to_string().len();
        if size > DATA_MAX {
            return Err(EmitError::TooLarge {
                name: name.to_owned(),
                size,
            });
        }
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        if held.len() >= STEP_EVENTS_MAX {
            return Err(EmitError::TooManyInStep);
        }
        // One slot of the transition's budget, taken only if there is one: a refused emit takes none.
        let mut used = self.transition.load(Ordering::SeqCst);
        loop {
            if used >= TRANSITION_EVENTS_MAX {
                return Err(EmitError::TooManyInTransition);
            }
            match self.transition.compare_exchange_weak(
                used,
                used + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(now) => used = now,
            }
        }
        held.push(RecordedEvent {
            name: spec.name.clone(),
            version: spec.version,
            data,
        });
        Ok(())
    }

    /// What the call has emitted, in order.
    pub(crate) fn emitted(&self) -> Vec<RecordedEvent> {
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

fn scrub_strings(value: &mut Value, scrub: &dyn Fn(&str) -> String) {
    match value {
        Value::String(text) => *text = scrub(text),
        Value::Array(items) => items.iter_mut().for_each(|v| scrub_strings(v, scrub)),
        Value::Object(map) => {
            let entries: Vec<(String, Value)> = std::mem::take(map).into_iter().collect();
            // Every scrubbed form is reserved first, so a suffixed name can never equal the scrubbed
            // form of a key that comes later.
            let mut taken: std::collections::HashSet<String> =
                entries.iter().map(|(key, _)| scrub(key)).collect();
            for (key, mut item) in entries {
                scrub_strings(&mut item, scrub);
                let scrubbed = scrub(&key);
                // Two keys that scrub to one form keep both: the first in the map's iteration order
                // (insertion order with serde_json's `preserve_order`, which the workspace enables
                // through feature unification; sorted order without it) keeps the name, each later one
                // gets `#2`, `#3`, ... The order is the input's, so the result is deterministic.
                let name = if map.contains_key(&scrubbed) {
                    let mut n = 2_usize;
                    loop {
                        let candidate = format!("{scrubbed}#{n}");
                        if taken.insert(candidate.clone()) {
                            break candidate;
                        }
                        n += 1;
                    }
                } else {
                    scrubbed
                };
                map.insert(name, item);
            }
        }
        _ => {}
    }
}

/// The events an agent declares: what each tool and the agent itself declared, checked once at build.
#[derive(Debug, Default)]
pub(crate) struct EventRegistry {
    /// Every declaration of the agent, by name: tools', agent's and plugins'.
    specs: BTreeMap<String, EventSpec>,
    /// What each tool may emit: its own declarations and the agent's.
    per_tool: BTreeMap<String, Arc<BTreeMap<String, EventSpec>>>,
}

impl EventRegistry {
    /// The registry of `tools` and the agent's own `agent` declarations, with `plugins`' declared
    /// beside them for the conflict check.
    ///
    /// # Errors
    /// The first declaration that is refused, or two of one name that differ, across all of them.
    pub(crate) fn build(
        tools: &[DynTool],
        agent: &[EventSpec],
        plugins: &[EventSpec],
    ) -> Result<Self, DeclarationError> {
        let own: Vec<(String, Vec<EventSpec>)> = tools
            .iter()
            .map(|tool| (tool.spec().name, tool.events()))
            .collect();
        let specs = declared(
            own.iter()
                .flat_map(|(_, specs)| specs)
                .chain(agent)
                .chain(plugins),
        )?;
        let shared: Vec<&EventSpec> = agent.iter().collect();
        let per_tool = own
            .iter()
            .map(|(tool, specs)| {
                let allowed: BTreeMap<String, EventSpec> = specs
                    .iter()
                    .chain(shared.iter().copied())
                    .map(|spec| (spec.name.clone(), spec.clone()))
                    .collect();
                (tool.clone(), Arc::new(allowed))
            })
            .collect();
        Ok(Self { specs, per_tool })
    }

    /// The declaration of `name`, if the agent has one.
    pub(crate) fn spec(&self, name: &str) -> Option<&EventSpec> {
        self.specs.get(name)
    }

    /// What `tool` may emit, with `transition` as the budget it spends from.
    pub(crate) fn scope(&self, tool: &str, transition: Arc<AtomicUsize>) -> Option<EventScope> {
        let allowed = self.per_tool.get(tool)?;
        (!allowed.is_empty()).then(|| EventScope::new(Arc::clone(allowed), transition))
    }

    /// The namespaces of every declaration.
    #[cfg(test)]
    pub(crate) fn namespaces(&self) -> std::collections::BTreeSet<String> {
        self.specs
            .values()
            .map(|s| s.namespace().to_owned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scope(names: &[&str], transition: &Arc<AtomicUsize>) -> EventScope {
        let allowed = names
            .iter()
            .map(|n| ((*n).to_owned(), EventSpec::new(*n)))
            .collect();
        EventScope::new(Arc::new(allowed), Arc::clone(transition))
    }

    fn keep(s: &str) -> String {
        s.to_owned()
    }

    #[test]
    fn keys_that_scrub_to_one_form_all_survive() {
        let scope = scope(&["demo.a"], &Arc::default());
        let scrub = |s: &str| s.replace("sk-one", "[x]").replace("sk-two", "[x]");
        // Two secret-bearing keys scrub to `[x]`, and a plain key is already named `[x]#2`.
        let data = json!({"sk-one": 1, "sk-two": 2, "[x]#2": 3});
        scope.emit("demo.a", data, &scrub).unwrap();
        let got = scope.emitted()[0].data.clone();
        let text = got.to_string();
        assert!(
            !text.contains("sk-one") && !text.contains("sk-two"),
            "{text}"
        );
        let obj = got.as_object().unwrap();
        assert_eq!(obj.len(), 3, "{text}");
        let mut values: Vec<i64> = obj.values().map(|v| v.as_i64().unwrap()).collect();
        values.sort_unstable();
        assert_eq!(values, [1, 2, 3]);
        assert_eq!(obj["[x]#2"], json!(3), "an existing key keeps its name");
        // The same input gives the same output.
        let again = EventScope::new(
            Arc::new([("demo.a".to_owned(), EventSpec::new("demo.a"))].into()),
            Arc::default(),
        );
        again
            .emit(
                "demo.a",
                json!({"sk-one": 1, "sk-two": 2, "[x]#2": 3}),
                &scrub,
            )
            .unwrap();
        assert_eq!(again.emitted()[0].data, got);
    }

    #[test]
    fn an_undeclared_event_is_refused_and_not_held() {
        let scope = scope(&["demo.a"], &Arc::default());
        assert_eq!(
            scope.emit("demo.b", json!({}), &keep),
            Err(EmitError::Undeclared("demo.b".into()))
        );
        assert!(scope.emitted().is_empty());
        assert!(scope.emit("demo.a", json!({"n": 1}), &keep).is_ok());
        assert_eq!(scope.emitted().len(), 1);
    }

    #[test]
    fn data_over_4_kib_is_refused_after_the_scrub() {
        let scope = scope(&["demo.a"], &Arc::default());
        let big = json!({"text": "x".repeat(DATA_MAX)});
        assert!(matches!(
            scope.emit("demo.a", big, &keep),
            Err(EmitError::TooLarge { .. })
        ));
        // A scrub that shrinks the data brings it back inside the bound.
        let secret = json!({"text": "SECRET".repeat(DATA_MAX / 6)});
        assert!(
            scope
                .emit("demo.a", secret, &|s| s.replace("SECRET", ""))
                .is_ok()
        );
        assert_eq!(scope.emitted()[0].data, json!({"text": ""}));
    }

    #[test]
    fn a_step_emits_sixteen_and_the_seventeenth_is_refused() {
        let scope = scope(&["demo.a"], &Arc::default());
        for _ in 0..STEP_EVENTS_MAX {
            scope.emit("demo.a", json!({}), &keep).unwrap();
        }
        assert_eq!(
            scope.emit("demo.a", json!({}), &keep),
            Err(EmitError::TooManyInStep)
        );
    }

    #[test]
    fn one_counter_serves_every_call_of_a_transition() {
        // Sixteen calls of sixteen events fill the 256; the 257th, from a call of its own, is refused
        // although that call has emitted nothing: a counter per call would let it through.
        let transition = Arc::new(AtomicUsize::new(0));
        for _ in 0..16 {
            let call = scope(&["demo.a"], &transition);
            for _ in 0..STEP_EVENTS_MAX {
                call.emit("demo.a", json!({}), &keep).unwrap();
            }
        }
        let last = scope(&["demo.a"], &transition);
        assert_eq!(
            last.emit("demo.a", json!({}), &keep),
            Err(EmitError::TooManyInTransition)
        );
        assert!(last.emitted().is_empty());
        assert_eq!(transition.load(Ordering::SeqCst), TRANSITION_EVENTS_MAX);
    }

    #[test]
    fn a_refused_event_spends_no_budget() {
        let transition = Arc::new(AtomicUsize::new(0));
        let scope = scope(&["demo.a"], &transition);
        let _ = scope.emit("demo.nope", json!({}), &keep);
        let _ = scope.emit("demo.a", json!({"x": "y".repeat(DATA_MAX)}), &keep);
        assert_eq!(transition.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_record_decodes_and_a_registry_knows_its_namespaces() {
        let event: RecordedEvent =
            serde_json::from_value(json!({"name": "demo.a", "version": 2, "data": {"k": 1}}))
                .unwrap();
        assert_eq!(event.version, 2);
        let registry = EventRegistry::build(&[], &[EventSpec::new("demo.a")], &[]).unwrap();
        assert_eq!(
            registry.namespaces(),
            std::collections::BTreeSet::from(["demo".to_owned()])
        );
        assert!(registry.spec("demo.a").is_some());
    }
}
