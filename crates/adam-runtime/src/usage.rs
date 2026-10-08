//! Usage: what each model call of a run cost, and the totals of a task.
//!
//! A [`UsageEvent`] says that one model call completed and how many tokens it took, under which step of
//! the task it ran (a subagent's) and with which model. A2A serves it with the `usage/v1` extension of
//! the orchestration layer (`adam-a2a-runtime`), to a client that activated it. [`UsageTotals`] are the
//! tokens of every call of a task, one entry per provider and model, which an agent keeps in its state
//! and the A2A server writes on the task when it ends or waits. The counts are [`Usage`]'s, AG-UI's
//! `TokenUsage` accounting. See
//! [ADR 0032](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0032-usage-per-model-call.md).
//!
//! Like every [`RunEvent`](crate::RunEvent) a usage event is best effort and not durable; the totals
//! are the record.

use adam_model::Usage;
use serde::{Deserialize, Serialize};

/// The longest a call id may be, in bytes: the contract's limit.
pub const MAX_USAGE_CALL_BYTES: usize = 128;

/// The longest a provider or model label may be, in bytes: the contract's limit. A longer one is cut
/// on a character boundary.
pub const MAX_USAGE_LABEL_BYTES: usize = 128;

/// The most entries [`UsageTotals`] keeps: the contract's limit, one per provider and model.
pub const MAX_USAGE_TOTALS: usize = 32;

/// One model call completed: what it cost and where it ran.
///
/// Make it with [`UsageEvent::new`] and the builder methods, which keep the contract's bounds (a call
/// id and labels of at most 128 bytes, cut on a character boundary).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UsageEvent {
    /// The call's id: unique within the task, the same every time the call is reported (a replay of
    /// its journal entry, a resubscribe), and another one for a call made again.
    pub call: String,
    /// The step of the task the call ran under: a subagent's step (`tool:<call id>` of the root
    /// run's call that started it). `None`: the agent's own call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// The provider as the model client knows it (`openai`), when it says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The model as configured: the alias the request named.
    pub model: String,
    /// The tokens of the call. A provider that answered without usage is zeros.
    pub usage: Usage,
    /// The model's context window as the deployment configured it, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
}

impl UsageEvent {
    /// The call `call` to `model` took `usage`. Both strings are cut to 128 bytes.
    pub fn new(call: impl AsRef<str>, model: impl AsRef<str>, usage: Usage) -> Self {
        Self {
            call: cut(call.as_ref(), MAX_USAGE_CALL_BYTES),
            step: None,
            provider: None,
            model: cut(model.as_ref(), MAX_USAGE_LABEL_BYTES),
            usage,
            context_window: None,
        }
    }

    /// Ran under the step `step` of the task (a subagent's).
    #[must_use]
    pub fn under(mut self, step: impl AsRef<str>) -> Self {
        self.step = Some(cut(step.as_ref(), MAX_USAGE_CALL_BYTES));
        self
    }

    /// Served by `provider`, a lower-case label.
    #[must_use]
    pub fn with_provider(mut self, provider: impl AsRef<str>) -> Self {
        self.provider = Some(cut(provider.as_ref(), MAX_USAGE_LABEL_BYTES));
        self
    }

    /// On a model whose context window is `window` tokens.
    #[must_use]
    pub fn with_context_window(mut self, window: u64) -> Self {
        self.context_window = Some(window);
        self
    }
}

/// The tokens of one provider and model in [`UsageTotals`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UsageTotal {
    /// The provider, when the model client said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The model as configured.
    pub model: String,
    /// Every call of this provider and model, summed.
    pub usage: Usage,
}

/// The tokens of every model call of a task, one entry per provider and model, at most
/// [`MAX_USAGE_TOTALS`] entries, in the order each first appeared.
///
/// An agent keeps it in its state, so it is as durable as the state: a task taken up again after a
/// question goes on from what it had, and the A2A server reads it from the run's state (the member
/// `usage_totals`) when the task ends or waits. Serialized as the list of entries.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UsageTotals(Vec<UsageTotal>);

impl UsageTotals {
    /// No calls yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether no call was counted.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The entries, in the order each first appeared.
    pub fn entries(&self) -> &[UsageTotal] {
        &self.0
    }

    /// Count a call of `model` served by `provider`. The labels are cut to 128 bytes. A provider and
    /// model that would be a 33rd entry is not counted (and logged): the contract keeps 32.
    pub fn add(&mut self, provider: Option<&str>, model: &str, usage: Usage) {
        let provider = provider.map(|p| cut(p, MAX_USAGE_LABEL_BYTES));
        let model = cut(model, MAX_USAGE_LABEL_BYTES);
        if let Some(entry) = self
            .0
            .iter_mut()
            .find(|e| e.provider == provider && e.model == model)
        {
            entry.usage = entry.usage.saturating_add(usage);
            return;
        }
        if self.0.len() >= MAX_USAGE_TOTALS {
            tracing::warn!(
                %model,
                "a task's usage keeps {MAX_USAGE_TOTALS} providers and models; this call is not in its totals"
            );
            return;
        }
        self.0.push(UsageTotal {
            provider,
            model,
            usage,
        });
    }

    /// Count the call `event` reports.
    pub fn add_event(&mut self, event: &UsageEvent) {
        self.add(event.provider.as_deref(), &event.model, event.usage);
    }

    /// Count every call of `other` (a child run's totals, when the child answers).
    pub fn merge(&mut self, other: &UsageTotals) {
        for entry in &other.0 {
            self.add(entry.provider.as_deref(), &entry.model, entry.usage);
        }
    }
}

/// `text` cut to at most `max` bytes, on a character boundary.
fn cut(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn an_event_keeps_the_bounds_and_says_only_what_is_true() {
        let event = UsageEvent::new("r-c0-1a2b3c4d", "glm-5.3", Usage::new(10, 2));
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            json!({"call": "r-c0-1a2b3c4d", "model": "glm-5.3",
                   "usage": {"input_tokens": 10, "output_tokens": 2}})
        );
        let full = event
            .under("tool:c2")
            .with_provider("openai")
            .with_context_window(131_072);
        let back: UsageEvent =
            serde_json::from_value(serde_json::to_value(&full).unwrap()).unwrap();
        assert_eq!(back, full);

        // Labels and ids are cut to 128 bytes, never inside a character.
        let long = "é".repeat(100);
        let cut = UsageEvent::new(&long, &long, Usage::default()).with_provider(&long);
        for text in [&cut.call, &cut.model, cut.provider.as_ref().unwrap()] {
            assert_eq!(text.len(), 128, "{text}");
            assert!(text.chars().all(|c| c == 'é'));
        }
    }

    #[test]
    fn totals_are_one_entry_per_provider_and_model() {
        let mut totals = UsageTotals::new();
        assert!(totals.is_empty());
        totals.add(
            Some("openai"),
            "glm",
            Usage::new(10, 1).with_reasoning_tokens(1),
        );
        totals.add(
            Some("openai"),
            "glm",
            Usage::new(20, 2).with_cached_input_tokens(5),
        );
        totals.add(Some("openai"), "small", Usage::new(1, 1));
        totals.add(None, "glm", Usage::new(1, 0));
        let entries = totals.entries();
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries[0].usage,
            Usage::new(30, 3)
                .with_reasoning_tokens(1)
                .with_cached_input_tokens(5)
        );
        assert_eq!(entries[1].model, "small");
        assert_eq!(entries[2].provider, None);

        let mut parent = UsageTotals::new();
        parent.add(Some("openai"), "small", Usage::new(100, 10));
        parent.merge(&totals);
        assert_eq!(parent.entries().len(), 3);
        assert_eq!(parent.entries()[0].usage, Usage::new(101, 11));

        // Serialized as the list, and read back.
        let json = serde_json::to_value(&parent).unwrap();
        assert!(json.is_array(), "{json}");
        assert_eq!(serde_json::from_value::<UsageTotals>(json).unwrap(), parent);
    }

    #[test]
    fn totals_keep_at_most_32_entries() {
        let mut totals = UsageTotals::new();
        for n in 0..MAX_USAGE_TOTALS + 3 {
            totals.add(Some("openai"), &format!("m{n}"), Usage::new(1, 1));
        }
        assert_eq!(totals.entries().len(), MAX_USAGE_TOTALS);
        // One already there still counts.
        totals.add(Some("openai"), "m0", Usage::new(1, 1));
        assert_eq!(totals.entries()[0].usage, Usage::new(2, 2));
    }
}
