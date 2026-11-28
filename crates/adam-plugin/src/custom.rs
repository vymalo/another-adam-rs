//! Custom events and subscriptions (ADR 0050, slice **H5**): what a tool, an agent or a plugin
//! declares before it emits, the names it may not use, and the patterns an observer subscribes with.
//!
//! An event is **declared** as an [`EventSpec`] (`name`, `version`, `schema`, [`Visibility`]) and
//! only a declared event can be emitted. The name is `ns.name` with up to four more segments, each
//! `[a-z][a-z0-9_]*`; the first segment is the **namespace**, and the namespaces of the core
//! catalogue ([`RESERVED_NAMESPACES`]) are refused so a declaration cannot pose as a core event.
//! Two declarations of one name must be equal.
//!
//! [`Visibility::Hooks`] (the default) keeps the event in the process: only observers read it.
//! [`Visibility::Clients`] also sends it to clients as the `RunEvent::Custom` the A2A layer already
//! serves as a data part, so no extension and no new event variant is needed.
//!
//! An observer subscribes with [`Subscriptions`]: an exact name, `ns.*` or `*`. A pattern whose
//! namespace is neither core nor declared can never match, and is refused at build.

use std::collections::{BTreeMap, BTreeSet};

use adam_error::{Classify, ErrorClass};
use serde_json::{Value, json};

use crate::{EventKind, HookEvent};

/// The namespaces of the core catalogue: no declared event may start with one of them (checked on
/// the first segment, without regard to case).
pub const RESERVED_NAMESPACES: [&str; 16] = [
    "run",
    "message",
    "turn",
    "model",
    "tool",
    "permission",
    "question",
    "approval",
    "compaction",
    "child",
    "session",
    "usage",
    "artifact",
    "exec",
    "hook",
    "adam",
];

/// The most data one custom event carries, serialized, in bytes: 4 KiB.
pub const DATA_MAX: usize = 4 * 1024;

/// The most custom events one step (one tool call) emits: 16.
pub const STEP_EVENTS_MAX: usize = 16;

/// The most custom events one transition emits, across all its calls: 256.
pub const TRANSITION_EVENTS_MAX: usize = 256;

/// Who may see a custom event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Visibility {
    /// Only observers in the process read it. The default.
    #[default]
    Hooks,
    /// Observers read it, and it also goes to clients as a `RunEvent::Custom`.
    Clients,
}

/// A custom event as it is declared: the name it is emitted under, the version of its data, the
/// JSON Schema of its data (what a consumer reads; the loop bounds the data's size, it does not
/// validate against the schema) and who may see it.
#[derive(Debug, Clone, PartialEq)]
pub struct EventSpec {
    /// `ns.name`, up to five segments, each `[a-z][a-z0-9_]*`, the namespace not a core one.
    pub name: String,
    /// The version of the event's data, at least 1.
    pub version: u32,
    /// The JSON Schema of the data.
    pub schema: Value,
    /// Who may see the event.
    pub visibility: Visibility,
}

/// An action a plugin declares (ADR 0050, decision 11, slice H1d): the name it is performed under,
/// the version of its declaration, and the JSON Schema of the data the action carries (`Value`
/// [`Plugin::perform`](crate::Plugin::perform) returns). The name rules and the namespace check
/// are the same as for [`EventSpec`]: `ns.name`, two to five segments of `[a-z][a-z0-9_]*`, and the
/// namespace is not one of the core catalogue's. The loop bounds the data's size; the schema is
/// carried for a consumer.
///
/// The plugin that declares the action is the one that performs it: `Plugin::actions` returns the
/// list, and `Plugin::perform` runs the one the loop asked for in the journaled step
/// `action:<turn>:<i>`.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionSpec {
    /// `ns.name`, up to five segments, each `[a-z][a-z0-9_]*`, the namespace not a core one.
    pub name: String,
    /// The version of the declaration, at least 1.
    pub version: u32,
    /// The JSON Schema of the data the action carries. The loop bounds the size; the schema is
    /// carried for a consumer.
    pub schema: Value,
}

impl ActionSpec {
    /// An action called `name`: version 1, any data.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: 1,
            schema: json!({}),
        }
    }

    /// The same action with the version of its declaration.
    #[must_use]
    pub fn with_version(mut self, version: u32) -> Self {
        self.version = version;
        self
    }

    /// The same action with the JSON Schema of its data.
    #[must_use]
    pub fn with_schema(mut self, schema: Value) -> Self {
        self.schema = schema;
        self
    }

    /// The namespace: the first segment of the name.
    #[must_use]
    pub fn namespace(&self) -> &str {
        namespace_of(&self.name)
    }

    /// Whether the declaration is allowed: a name of the right shape outside the core namespaces,
    /// and a version of at least 1.
    ///
    /// # Errors
    /// [`DeclarationError::Reserved`], [`DeclarationError::BadName`] or
    /// [`DeclarationError::BadVersion`].
    pub fn validate(&self) -> Result<(), DeclarationError> {
        check_event_name(&self.name)?;
        if self.version == 0 {
            return Err(DeclarationError::BadVersion {
                name: self.name.clone(),
            });
        }
        Ok(())
    }
}

impl EventSpec {
    /// An event called `name`: version 1, any data, [`Visibility::Hooks`].
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: 1,
            schema: json!({}),
            visibility: Visibility::Hooks,
        }
    }

    /// The same event, sent to clients too ([`Visibility::Clients`]).
    #[must_use]
    pub fn published(mut self) -> Self {
        self.visibility = Visibility::Clients;
        self
    }

    /// The same event with the version of its data.
    #[must_use]
    pub fn with_version(mut self, version: u32) -> Self {
        self.version = version;
        self
    }

    /// The same event with the JSON Schema of its data.
    #[must_use]
    pub fn with_schema(mut self, schema: Value) -> Self {
        self.schema = schema;
        self
    }

    /// The same event with `visibility`.
    #[must_use]
    pub fn with_visibility(mut self, visibility: Visibility) -> Self {
        self.visibility = visibility;
        self
    }

    /// The namespace: the first segment of the name.
    #[must_use]
    pub fn namespace(&self) -> &str {
        namespace_of(&self.name)
    }

    /// Whether the declaration is allowed: a name of the right shape outside the core namespaces,
    /// and a version of at least 1.
    ///
    /// # Errors
    /// [`DeclarationError::Reserved`], [`DeclarationError::BadName`] or
    /// [`DeclarationError::BadVersion`].
    pub fn validate(&self) -> Result<(), DeclarationError> {
        check_event_name(&self.name)?;
        if self.version == 0 {
            return Err(DeclarationError::BadVersion {
                name: self.name.clone(),
            });
        }
        Ok(())
    }
}

/// Why a declaration or a subscription is refused. Every case is a build error: the class is
/// [`ErrorClass::Invalid`], and a binary exits 78 on it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DeclarationError {
    /// The name is not `ns.name` of up to five `[a-z][a-z0-9_]*` segments.
    #[error("the event name `{name}` is not `ns.name` (two to five segments of [a-z][a-z0-9_]*)")]
    BadName {
        /// The refused name.
        name: String,
    },
    /// The namespace is one of the core catalogue's.
    #[error("the event `{name}` is in the reserved namespace `{namespace}`: pick your own")]
    Reserved {
        /// The refused name.
        name: String,
        /// Its (core) namespace.
        namespace: String,
    },
    /// The version is zero.
    #[error("the event `{name}` has version 0: versions start at 1")]
    BadVersion {
        /// The event.
        name: String,
    },
    /// Two declarations of one name differ.
    #[error("the event `{name}` is declared twice, differently")]
    Conflict {
        /// The event.
        name: String,
    },
    /// A subscription pattern is not an exact name, `ns.*` or `*`.
    #[error("the subscription `{pattern}` of `{plugin}` is not a name, `ns.*` or `*`")]
    BadPattern {
        /// The plugin that subscribed.
        plugin: String,
        /// The refused pattern.
        pattern: String,
    },
    /// A subscription can never match: its namespace is neither core nor declared, or it names a core
    /// event that does not exist.
    #[error(
        "the subscription `{pattern}` of `{plugin}` matches nothing: no event is declared there"
    )]
    DeadSubscription {
        /// The plugin that subscribed.
        plugin: String,
        /// The pattern that is dead.
        pattern: String,
    },
}

impl Classify for DeclarationError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}

/// The first segment of an event name.
fn namespace_of(name: &str) -> &str {
    name.split('.').next().unwrap_or(name)
}

fn is_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Whether `name` has the shape `^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*){1,4}$`. A shape only: it does
/// not know the core namespaces, so a subscription to `tool.completed` passes.
#[must_use]
pub fn has_event_name_shape(name: &str) -> bool {
    let segments: Vec<&str> = name.split('.').collect();
    (2..=5).contains(&segments.len()) && segments.iter().all(|s| is_segment(s))
}

fn is_reserved(namespace: &str) -> bool {
    RESERVED_NAMESPACES
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(namespace))
}

/// Whether a name may be **declared**: the shape of [`has_event_name_shape`], and a namespace that
/// is not a core one (compared without regard to case, on the first segment only).
///
/// # Errors
/// [`DeclarationError::Reserved`] (checked first, so `Tool.x` says "reserved") or
/// [`DeclarationError::BadName`].
pub fn check_event_name(name: &str) -> Result<(), DeclarationError> {
    let namespace = namespace_of(name);
    if name.contains('.') && is_reserved(namespace) {
        return Err(DeclarationError::Reserved {
            name: name.to_owned(),
            namespace: namespace.to_owned(),
        });
    }
    if has_event_name_shape(name) {
        Ok(())
    } else {
        Err(DeclarationError::BadName {
            name: name.to_owned(),
        })
    }
}

/// Every declaration checked, and the set of them by name.
///
/// # Errors
/// The first declaration that is refused ([`EventSpec::validate`]), or two of one name that differ
/// ([`DeclarationError::Conflict`]). The same declaration made twice is one.
pub fn declared<'a>(
    specs: impl IntoIterator<Item = &'a EventSpec>,
) -> Result<BTreeMap<String, EventSpec>, DeclarationError> {
    let mut all: BTreeMap<String, EventSpec> = BTreeMap::new();
    for spec in specs {
        spec.validate()?;
        match all.get(&spec.name) {
            Some(known) if known != spec => {
                return Err(DeclarationError::Conflict {
                    name: spec.name.clone(),
                });
            }
            Some(_) => {}
            None => {
                all.insert(spec.name.clone(), spec.clone());
            }
        }
    }
    Ok(all)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Pattern {
    All,
    Namespace(String),
    Exact(String),
}

impl Pattern {
    fn parse(plugin: &str, pattern: &str) -> Result<Self, DeclarationError> {
        let bad = || DeclarationError::BadPattern {
            plugin: plugin.to_owned(),
            pattern: pattern.to_owned(),
        };
        if pattern == "*" {
            return Ok(Pattern::All);
        }
        if let Some(namespace) = pattern.strip_suffix(".*") {
            return if is_segment(namespace) {
                Ok(Pattern::Namespace(namespace.to_owned()))
            } else {
                Err(bad())
            };
        }
        if has_event_name_shape(pattern) {
            Ok(Pattern::Exact(pattern.to_owned()))
        } else {
            Err(bad())
        }
    }

    fn text(&self) -> String {
        match self {
            Pattern::All => "*".to_owned(),
            Pattern::Namespace(namespace) => format!("{namespace}.*"),
            Pattern::Exact(name) => name.clone(),
        }
    }

    fn matches(&self, name: &str) -> bool {
        match self {
            Pattern::All => true,
            Pattern::Namespace(namespace) => namespace_of(name) == namespace && name.contains('.'),
            Pattern::Exact(exact) => exact == name,
        }
    }
}

/// What an observer wants to be told: the core catalogue, patterns, or nothing.
///
/// A pattern is an exact name (`tool.completed`, `demo.published`), a namespace (`demo.*`) or
/// everything (`*`). [`Subscriptions::core`], the default of [`Plugin::subscriptions`](crate::Plugin::subscriptions),
/// is every event of the core catalogue and no custom one: an observer written before custom events
/// existed receives what it always did, and a custom event reaches only an observer that asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscriptions {
    core: bool,
    patterns: Vec<Pattern>,
}

impl Subscriptions {
    /// Nothing: the observer is never offered an event.
    #[must_use]
    pub fn none() -> Self {
        Self {
            core: false,
            patterns: Vec::new(),
        }
    }

    /// Every core event and no custom one: what an observer received before custom events.
    #[must_use]
    pub fn core() -> Self {
        Self {
            core: true,
            patterns: Vec::new(),
        }
    }

    /// Every event, core and custom (`*`).
    #[must_use]
    pub fn all() -> Self {
        Self {
            core: false,
            patterns: vec![Pattern::All],
        }
    }

    /// Exactly these patterns, and no core event unless one of them matches it.
    ///
    /// # Errors
    /// [`DeclarationError::BadPattern`] for a pattern that is not an exact name, `ns.*` or `*`.
    pub fn of<S: AsRef<str>>(
        patterns: impl IntoIterator<Item = S>,
    ) -> Result<Self, DeclarationError> {
        patterns
            .into_iter()
            .try_fold(Self::none(), |subscriptions, pattern| {
                subscriptions.with(pattern.as_ref())
            })
    }

    /// The same subscriptions, and `pattern` too.
    ///
    /// # Errors
    /// [`DeclarationError::BadPattern`].
    pub fn with(mut self, pattern: &str) -> Result<Self, DeclarationError> {
        let pattern = Pattern::parse("", pattern)?;
        if !self.patterns.contains(&pattern) {
            self.patterns.push(pattern);
        }
        Ok(self)
    }

    /// Whether the observer is offered `event`.
    #[must_use]
    pub fn matches(&self, event: &HookEvent) -> bool {
        if self.core && event.kind != EventKind::Custom {
            return true;
        }
        let name = event.name();
        self.patterns.iter().any(|pattern| pattern.matches(name))
    }

    /// Whether every pattern can match something: its namespace is a core one or is in `declared`
    /// (the namespaces of the events some tool, agent or plugin declares), and an exact core name is
    /// in the catalogue. `plugin` names the plugin in the error.
    ///
    /// # Errors
    /// [`DeclarationError::DeadSubscription`].
    pub fn check(&self, plugin: &str, declared: &BTreeSet<String>) -> Result<(), DeclarationError> {
        for pattern in &self.patterns {
            let live = match pattern {
                Pattern::All => true,
                Pattern::Namespace(namespace) => {
                    is_reserved_exact(namespace) || declared.contains(namespace)
                }
                Pattern::Exact(name) => {
                    let namespace = namespace_of(name);
                    if is_reserved_exact(namespace) {
                        EventKind::ALL.iter().any(|kind| kind.name() == name)
                    } else {
                        declared.contains(namespace)
                    }
                }
            };
            if !live {
                return Err(DeclarationError::DeadSubscription {
                    plugin: plugin.to_owned(),
                    pattern: pattern.text(),
                });
            }
        }
        Ok(())
    }
}

impl Default for Subscriptions {
    fn default() -> Self {
        Self::core()
    }
}

fn is_reserved_exact(namespace: &str) -> bool {
    RESERVED_NAMESPACES.contains(&namespace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventHeader;

    fn header() -> EventHeader {
        EventHeader {
            key: "k".into(),
            run: "r".into(),
            root_run: "r".into(),
            agent: "a".into(),
            at: "t".into(),
        }
    }

    fn custom(name: &str) -> HookEvent {
        HookEvent::custom(name, 1, header(), &json!({}), &|s| s.to_owned())
    }

    #[test]
    fn a_name_has_two_to_five_lowercase_segments() {
        for ok in ["demo.published", "a.b", "a1.b_2.c.d.e"] {
            assert!(check_event_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "demo",
            "",
            "demo.",
            ".demo",
            "demo..x",
            "Demo.x",
            "demo.X",
            "1demo.x",
            "demo.x-y",
            "a.b.c.d.e.f",
            "demo.x ",
        ] {
            assert!(
                matches!(check_event_name(bad), Err(DeclarationError::BadName { .. })),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_core_namespace_is_refused_on_the_first_segment_without_regard_to_case() {
        for namespace in RESERVED_NAMESPACES {
            let name = format!("{namespace}.mine");
            assert!(
                matches!(
                    check_event_name(&name),
                    Err(DeclarationError::Reserved { .. })
                ),
                "{name}"
            );
        }
        for name in ["Tool.mine", "TOOL.mine", "Adam.x"] {
            assert!(
                matches!(
                    check_event_name(name),
                    Err(DeclarationError::Reserved { .. })
                ),
                "{name}"
            );
        }
        // Only the first segment is a namespace; a core word deeper in is fine.
        assert!(check_event_name("demo.tool.completed").is_ok());
        assert!(check_event_name("coder.files.changed").is_ok());
    }

    #[test]
    fn a_declaration_needs_a_version_and_two_of_a_name_must_be_equal() {
        assert!(matches!(
            EventSpec::new("demo.x").with_version(0).validate(),
            Err(DeclarationError::BadVersion { .. })
        ));
        let a = EventSpec::new("demo.x");
        assert_eq!(declared([&a, &a.clone()]).unwrap().len(), 1);
        let b = EventSpec::new("demo.x").published();
        assert!(matches!(
            declared([&a, &b]),
            Err(DeclarationError::Conflict { .. })
        ));
        let c = EventSpec::new("demo.x").with_version(2);
        assert!(matches!(
            declared([&a, &c]),
            Err(DeclarationError::Conflict { .. })
        ));
    }

    /// An action is declared like a custom event: the name rules and the namespace check are the
    /// same, and a version of 0 is refused the same way.
    #[test]
    fn an_action_is_declared_like_an_event() {
        assert!(ActionSpec::new("demo.x").validate().is_ok());
        assert!(ActionSpec::new("demo.x").with_version(2).validate().is_ok());
        assert!(matches!(
            ActionSpec::new("demo.x").with_version(0).validate(),
            Err(DeclarationError::BadVersion { .. })
        ));
        assert!(matches!(
            ActionSpec::new("tool.demo").validate(),
            Err(DeclarationError::Reserved { .. })
        ));
        assert!(matches!(
            ActionSpec::new("demo").validate(),
            Err(DeclarationError::BadName { .. })
        ));
        assert_eq!(ActionSpec::new("demo.x").namespace(), "demo");
    }

    #[test]
    fn patterns_match_exact_names_namespaces_and_everything() {
        let exact = Subscriptions::of(["demo.published"]).unwrap();
        assert!(exact.matches(&custom("demo.published")));
        assert!(!exact.matches(&custom("demo.kept")));
        let namespace = Subscriptions::of(["demo.*"]).unwrap();
        assert!(namespace.matches(&custom("demo.kept")));
        assert!(namespace.matches(&custom("demo.a.b")));
        assert!(!namespace.matches(&custom("other.kept")));
        assert!(!namespace.matches(&custom("demonic.kept")));
        assert!(Subscriptions::all().matches(&custom("other.kept")));
        assert!(!Subscriptions::none().matches(&custom("demo.kept")));
    }

    #[test]
    fn the_default_is_the_core_catalogue_and_never_a_custom_event() {
        let core = Subscriptions::default();
        assert!(!core.matches(&custom("demo.published")));
        let completed = HookEvent::new(EventKind::ToolCompleted, header());
        assert!(core.matches(&completed));
        assert!(Subscriptions::all().matches(&completed));
        // Patterns alone leave the core out unless one of them says it.
        assert!(!Subscriptions::of(["demo.*"]).unwrap().matches(&completed));
        assert!(
            Subscriptions::of(["tool.completed"])
                .unwrap()
                .matches(&completed)
        );
        assert!(Subscriptions::of(["tool.*"]).unwrap().matches(&completed));
        assert!(
            Subscriptions::core()
                .with("demo.*")
                .unwrap()
                .matches(&custom("demo.x"))
        );
    }

    #[test]
    fn a_subscription_to_a_core_event_is_legal_though_a_declaration_there_is_not() {
        assert!(Subscriptions::of(["tool.completed"]).is_ok());
        assert!(check_event_name("tool.completed").is_err());
    }

    #[test]
    fn a_pattern_that_is_not_a_pattern_is_refused() {
        for bad in [
            "", "demo", "demo.*.x", "*.x", "Demo.*", "demo.x*", "**", "demo.*.",
        ] {
            assert!(
                matches!(
                    Subscriptions::of([bad]),
                    Err(DeclarationError::BadPattern { .. })
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_dead_subscription_is_refused() {
        let declared: BTreeSet<String> = ["demo".to_owned()].into();
        let check = |p: &str| Subscriptions::of([p]).unwrap().check("p", &declared);
        for live in [
            "*",
            "demo.*",
            "demo.anything",
            "tool.*",
            "tool.completed",
            "session.round.started",
        ] {
            assert!(check(live).is_ok(), "{live}");
        }
        for dead in [
            "nothing.*",
            "nothing.x",
            "tool.bogus",
            "tool.completed.extra",
        ] {
            assert!(
                matches!(check(dead), Err(DeclarationError::DeadSubscription { .. })),
                "{dead}"
            );
        }
        assert!(Subscriptions::core().check("p", &BTreeSet::new()).is_ok());
    }
}
