//! Shared state for tools: a typed map, and the handle a tool receives.

use std::any::{Any, TypeId, type_name};
use std::collections::HashMap;
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

/// Names one type of shared state: what [`Tool::required_state`](crate::Tool::required_state)
/// returns, and what
/// [`LlmAgentBuilder::try_build`](crate::LlmAgentBuilder::try_build) checks against the
/// agent's [`Extensions`].
///
/// Compares by type, prints as the type's name.
#[derive(Clone, Copy)]
pub struct StateKey {
    id: TypeId,
    name: &'static str,
}

impl StateKey {
    /// The key of `T`.
    pub fn of<T: Send + Sync + 'static>() -> Self {
        Self {
            id: TypeId::of::<T>(),
            name: type_name::<T>(),
        }
    }

    /// The type's name, for messages. Not stable across compiler versions:
    /// never parse it.
    pub fn type_name(&self) -> &'static str {
        self.name
    }
}

impl PartialEq for StateKey {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for StateKey {}

impl std::hash::Hash for StateKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl fmt::Debug for StateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StateKey({})", self.name)
    }
}

impl fmt::Display for StateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}

/// A map from a type to one shared value of that type (at most one per type).
///
/// The agent owns one ([`LlmAgentBuilder::state`](crate::LlmAgentBuilder::state)) and every
/// [`ToolCtx`](crate::ToolCtx) it builds carries it, so a tool reads its
/// dependencies with [`ToolCtx::state`](crate::ToolCtx::state). Values are
/// shared (`Arc`), never cloned.
#[derive(Clone, Default)]
pub struct Extensions {
    map: HashMap<TypeId, Entry>,
}

#[derive(Clone)]
struct Entry {
    name: &'static str,
    value: Arc<dyn Any + Send + Sync>,
}

impl Extensions {
    /// An empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Store `value`, returning the value of the same type it replaced.
    pub fn insert<T: Send + Sync + 'static>(&mut self, value: Arc<T>) -> Option<Arc<T>> {
        let previous = self.map.insert(
            TypeId::of::<T>(),
            Entry {
                name: type_name::<T>(),
                value,
            },
        );
        previous.and_then(|entry| entry.value.downcast::<T>().ok())
    }

    /// The value of type `T`, if one was stored.
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.map
            .get(&TypeId::of::<T>())
            .and_then(|entry| Arc::clone(&entry.value).downcast::<T>().ok())
    }

    /// Whether a value for `key` was stored.
    pub fn contains(&self, key: StateKey) -> bool {
        self.map.contains_key(&key.id)
    }

    /// How many values are stored.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether nothing is stored.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl fmt::Debug for Extensions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut names: Vec<&str> = self.map.values().map(|e| e.name).collect();
        names.sort_unstable();
        f.debug_set().entries(names).finish()
    }
}

/// Shared state as a tool sees it: a cheap-to-clone handle to a `T` that the
/// agent was given with
/// [`LlmAgentBuilder::state`](crate::LlmAgentBuilder::state). Derefs to `T`.
///
/// Get one with [`ToolCtx::state`](crate::ToolCtx::state) (or
/// [`require_state`](crate::ToolCtx::require_state)), and declare the
/// dependency in [`Tool::required_state`](crate::Tool::required_state) so a
/// missing value fails when the agent is built and not in the middle of a run.
pub struct State<T>(Arc<T>);

impl<T> State<T> {
    /// Wrap a value.
    pub fn new(value: T) -> Self {
        Self(Arc::new(value))
    }

    /// The shared value itself.
    pub fn into_inner(self) -> Arc<T> {
        self.0
    }
}

impl<T> From<Arc<T>> for State<T> {
    fn from(value: Arc<T>) -> Self {
        Self(value)
    }
}

impl<T> Clone for State<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> Deref for State<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: fmt::Debug> fmt::Debug for State<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("State").field(&*self.0).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Db(&'static str);
    struct Clock;

    #[test]
    fn a_value_is_found_by_its_type_and_replaced_by_the_same_type() {
        let mut map = Extensions::new();
        assert!(map.is_empty());
        assert!(map.insert(Arc::new(Db("a"))).is_none());
        assert!(map.insert(Arc::new(Clock)).is_none());
        assert_eq!(map.len(), 2);
        assert_eq!(map.get::<Db>().as_deref(), Some(&Db("a")));

        let old = map.insert(Arc::new(Db("b")));
        assert_eq!(old.as_deref(), Some(&Db("a")));
        assert_eq!(map.get::<Db>().as_deref(), Some(&Db("b")));
        assert_eq!(map.len(), 2);
        assert!(map.get::<String>().is_none());
    }

    #[test]
    fn keys_compare_by_type_and_print_the_type_name() {
        assert_eq!(StateKey::of::<Db>(), StateKey::of::<Db>());
        assert_ne!(StateKey::of::<Db>(), StateKey::of::<Clock>());
        assert!(StateKey::of::<Db>().to_string().ends_with("Db"));
        assert!(format!("{:?}", StateKey::of::<Db>()).starts_with("StateKey("));
        let mut map = Extensions::new();
        map.insert(Arc::new(Db("a")));
        assert!(map.contains(StateKey::of::<Db>()));
        assert!(!map.contains(StateKey::of::<Clock>()));
        assert!(format!("{map:?}").contains("Db"));
    }

    #[test]
    fn state_derefs_and_clones_without_copying_the_value() {
        let state = State::new(Db("x"));
        assert_eq!((*state).0, "x");
        let again = state.clone();
        assert!(Arc::ptr_eq(&state.into_inner(), &again.into_inner()));
        let from_arc: State<Db> = Arc::new(Db("y")).into();
        assert!(format!("{from_arc:?}").contains('y'));
    }
}
