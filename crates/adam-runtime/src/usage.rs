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
//!
//! [`SandboxUsage`] is the other cost of a run: the time its commands ran in a sandbox, weighted by the
//! shape of the sandbox. A tool reports it through the tool context, the loop keeps it in the run's state
//! beside the totals, and the A2A server writes it on the task beside them (ADR 0043).

use adam_model::Usage;
use serde::{Deserialize, Serialize};

/// The longest a call id may be, in bytes: the contract's limit.
pub const MAX_USAGE_CALL_BYTES: usize = 128;

/// The longest a provider or model label may be, in bytes: the contract's limit. A longer one is cut
/// on a character boundary.
pub const MAX_USAGE_LABEL_BYTES: usize = 128;

/// The most entries [`UsageTotals`] keeps: the contract's limit, one per provider and model.
pub const MAX_USAGE_TOTALS: usize = 32;

/// The most entries [`SandboxUsage`] keeps: the contract's limit, one per shape.
pub const MAX_SANDBOX_ENTRIES: usize = 8;

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

/// Where a run's commands ran, as far as a report says.
///
/// Ordered by how much is known: when two reports of one run disagree (a deployment that changed its
/// environment between two turns), the one that measures wins, so measured time is never hidden.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum SandboxEnvironment {
    /// The coder's own container: nothing is measured.
    Local,
    /// A dev container made from a repository's `devcontainer.json` or a default image: nothing is
    /// measured.
    Devcontainer,
    /// A pod of the run's own: the time of its commands is measured.
    Kubernetes,
}

impl SandboxEnvironment {
    /// The word the contract uses.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Devcontainer => "devcontainer",
            Self::Kubernetes => "kubernetes",
        }
    }

    /// Whether the time of commands is measured in this environment. When it is not, a report says
    /// where the run was and nothing else: *not measured*, which is not zero.
    pub fn is_measured(self) -> bool {
        match self {
            Self::Local | Self::Devcontainer => false,
            Self::Kubernetes => true,
        }
    }
}

/// The commands that ran in sandboxes of one shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SandboxEntry {
    /// What the sandbox reserved for CPU, in thousandths of a core (`250` is `250m`). `None`: the
    /// sandbox's template requests none, which is not zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_millis: Option<u64>,
    /// What the sandbox reserved for memory, in bytes. `None`: it requests none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    /// How many commands ran.
    pub commands: u64,
    /// The sum of their wall times, in milliseconds, from the start of each command to its exit
    /// (a command stopped by a timeout or a cancel counts until it was stopped). Commands that ran at
    /// the same time are summed, so this can be more than the time the run took.
    pub milliseconds: u64,
}

/// The time the commands of a run ran in a sandbox, one entry per shape of sandbox, at most
/// [`MAX_SANDBOX_ENTRIES`], in the order each first appeared; and the environment they ran in.
///
/// It is empty until something asks for an environment. A **measured** environment
/// ([`SandboxEnvironment::Kubernetes`]) with no entry is a run that asked for its pod and ran no
/// command: a measured zero. An environment that is not measured has no entry ever.
///
/// Like [`UsageTotals`] it is kept in an agent's state, so it is as durable as the state, and it is
/// merged into the parent's when a child run answers. Serialized as `{"environment": ..., "usage": [...]}`,
/// each member left out while it is empty.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    environment: Option<SandboxEnvironment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    usage: Vec<SandboxEntry>,
}

impl SandboxUsage {
    /// Nothing asked of an environment yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The run asked for its environment, `environment`, and ran nothing in it (yet).
    pub fn in_environment(environment: SandboxEnvironment) -> Self {
        Self {
            environment: Some(environment),
            usage: Vec::new(),
        }
    }

    /// One command of `milliseconds` ran in a sandbox of the given shape, in a measured environment.
    pub fn ran_command(
        cpu_millis: Option<u64>,
        memory_bytes: Option<u64>,
        milliseconds: u64,
    ) -> Self {
        Self {
            environment: Some(SandboxEnvironment::Kubernetes),
            usage: vec![SandboxEntry {
                cpu_millis,
                memory_bytes,
                commands: 1,
                milliseconds,
            }],
        }
    }

    /// Whether nothing was reported: no environment asked for, no command.
    pub fn is_empty(&self) -> bool {
        self.environment.is_none() && self.usage.is_empty()
    }

    /// The environment the run's commands ran in, when it asked for one.
    pub fn environment(&self) -> Option<SandboxEnvironment> {
        self.environment
    }

    /// The entries, in the order each shape first appeared.
    pub fn entries(&self) -> &[SandboxEntry] {
        &self.usage
    }

    /// Count everything `other` says (a call's report, a child run's). A shape that would be a 9th
    /// entry is not counted (and logged): the contract keeps 8, and a deployment has one template.
    pub fn merge(&mut self, other: &SandboxUsage) {
        self.environment = self.environment.max(other.environment);
        for entry in &other.usage {
            if let Some(mine) = self
                .usage
                .iter_mut()
                .find(|e| e.cpu_millis == entry.cpu_millis && e.memory_bytes == entry.memory_bytes)
            {
                mine.commands = mine.commands.saturating_add(entry.commands);
                mine.milliseconds = mine.milliseconds.saturating_add(entry.milliseconds);
            } else if self.usage.len() >= MAX_SANDBOX_ENTRIES {
                tracing::warn!(
                    "a task's sandbox usage keeps {MAX_SANDBOX_ENTRIES} shapes; these commands are not in it"
                );
            } else {
                self.usage.push(*entry);
            }
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

    #[test]
    fn sandbox_commands_of_one_shape_are_one_entry_and_a_shape_that_differs_is_another() {
        let mut usage = SandboxUsage::new();
        assert!(usage.is_empty() && usage.environment().is_none());
        usage.merge(&SandboxUsage::ran_command(
            Some(250),
            Some(536_870_912),
            1_000,
        ));
        usage.merge(&SandboxUsage::ran_command(
            Some(250),
            Some(536_870_912),
            2_500,
        ));
        usage.merge(&SandboxUsage::ran_command(Some(500), None, 40));
        assert_eq!(usage.environment(), Some(SandboxEnvironment::Kubernetes));
        let [first, second] = usage.entries() else {
            panic!("two shapes: {usage:?}");
        };
        assert_eq!((first.commands, first.milliseconds), (2, 3_500));
        assert_eq!((second.cpu_millis, second.memory_bytes), (Some(500), None));
        assert_eq!((second.commands, second.milliseconds), (1, 40));
    }

    #[test]
    fn a_childs_sandbox_usage_is_added_to_its_parents() {
        let mut parent = SandboxUsage::ran_command(Some(250), Some(1), 100);
        let mut child = SandboxUsage::ran_command(Some(250), Some(1), 50);
        child.merge(&SandboxUsage::ran_command(Some(1000), Some(2), 7));
        parent.merge(&child);
        assert_eq!(parent.entries().len(), 2);
        assert_eq!(parent.entries()[0].commands, 2);
        assert_eq!(parent.entries()[0].milliseconds, 150);
    }

    #[test]
    fn the_environment_that_measures_wins_and_asking_for_one_is_not_a_command() {
        let mut usage = SandboxUsage::in_environment(SandboxEnvironment::Kubernetes);
        assert!(!usage.is_empty());
        assert!(
            usage.entries().is_empty(),
            "a pod asked for, no command: a measured zero"
        );
        usage.merge(&SandboxUsage::in_environment(SandboxEnvironment::Local));
        assert_eq!(usage.environment(), Some(SandboxEnvironment::Kubernetes));

        let mut local = SandboxUsage::in_environment(SandboxEnvironment::Local);
        local.merge(&SandboxUsage::in_environment(
            SandboxEnvironment::Devcontainer,
        ));
        assert_eq!(local.environment(), Some(SandboxEnvironment::Devcontainer));
        assert!(!SandboxEnvironment::Local.is_measured());
        assert!(!SandboxEnvironment::Devcontainer.is_measured());
        assert!(SandboxEnvironment::Kubernetes.is_measured());
    }

    #[test]
    fn sandbox_usage_keeps_at_most_8_shapes_and_saturates() {
        let mut usage = SandboxUsage::new();
        for n in 0..MAX_SANDBOX_ENTRIES as u64 + 3 {
            usage.merge(&SandboxUsage::ran_command(Some(n), None, 1));
        }
        assert_eq!(usage.entries().len(), MAX_SANDBOX_ENTRIES);
        // A shape already there still counts, and the counts never wrap.
        usage.merge(&SandboxUsage::ran_command(Some(0), None, u64::MAX));
        usage.merge(&SandboxUsage::ran_command(Some(0), None, 5));
        assert_eq!(usage.entries()[0].milliseconds, u64::MAX);
        assert_eq!(usage.entries()[0].commands, 3);
    }

    #[test]
    fn sandbox_usage_serializes_what_it_knows_and_state_without_it_loads() {
        assert_eq!(
            serde_json::to_value(SandboxUsage::new()).unwrap(),
            json!({})
        );
        assert_eq!(
            serde_json::to_value(SandboxUsage::in_environment(SandboxEnvironment::Local)).unwrap(),
            json!({"environment": "local"})
        );
        let usage = SandboxUsage::ran_command(Some(250), None, 9);
        let json = serde_json::to_value(&usage).unwrap();
        assert_eq!(
            json,
            json!({"environment": "kubernetes",
                   "usage": [{"cpu_millis": 250, "commands": 1, "milliseconds": 9}]})
        );
        assert_eq!(serde_json::from_value::<SandboxUsage>(json).unwrap(), usage);
        // State written before the member existed has no key at all.
        assert_eq!(
            serde_json::from_value::<SandboxUsage>(json!({})).unwrap(),
            SandboxUsage::new()
        );
    }
}
