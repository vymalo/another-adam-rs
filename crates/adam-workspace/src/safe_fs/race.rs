//! A seam for tests that race the coder's own checks.
//!
//! A command in a run pod can rename a slot's directory, or a directory below it, and put a link at
//! the old name, at any moment (a process it leaves running outlives the command that started it).
//! The coder checks a path and then uses it, and the tests of this repository prove that nothing it
//! does follows such a link by making the swap happen **between** the check and the use: the code
//! calls [`checkpoint`] where a check ends and the use begins, and a test registers a closure
//! ([`on_checkpoint`]) that does what the run could do there.
//!
//! [`checkpoint`] does nothing unless the crate is built for its tests or with the `test-util`
//! feature: a release build has no registry, no lock and no closure to call.
//!
//! The closures are keyed by the slot's path (the one the slot was opened with) and not by thread:
//! the coder moves its filesystem work through `spawn_blocking`.
//!
//! The names of the checkpoints a test can wait for:
//!
//! | name | where |
//! |---|---|
//! | `read` | `read_file` and `share_file`: the path is resolved, the file is not yet opened |
//! | `write` | `write_file`: the directories are made, the new file is not yet created |
//! | `edit` | `edit_file`: the file is located, not yet read |
//! | `copy-source`, `copy-dest` | `copy_into`: the plan is made, no file is written yet |
//! | `restore-git` | undoing a command's change to a worktree's `.git` file |
//! | `exclude` | undoing a command's change to a scratch project's `info/exclude` |
//! | `git` | `git` is about to be started on a slot |

use std::path::Path;

/// A place where a check has ended and the use that follows it has not begun (see the [module
/// documentation](self)). `slot` is the slot's path; `at` names the place.
///
/// Does nothing without the `test-util` feature (and outside this crate's own tests).
#[inline]
pub fn checkpoint(slot: &Path, at: &'static str) {
    #[cfg(any(test, feature = "test-util"))]
    hooks::fire(slot, at);
    #[cfg(not(any(test, feature = "test-util")))]
    let _ = (slot, at);
}

#[cfg(any(test, feature = "test-util"))]
pub use hooks::{Hook, on_checkpoint};

#[cfg(any(test, feature = "test-util"))]
mod hooks {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

    thread_local! {
        /// The hooks whose closure this thread is running: a checkpoint reached from inside one
        /// skips it, where waiting for its own lock would never return.
        static RUNNING: RefCell<Vec<*const Registered>> = const { RefCell::new(Vec::new()) };
    }

    type Callback = Box<dyn FnMut(&'static str, usize) + Send>;

    struct Registered {
        callback: Mutex<Callback>,
        /// How often each checkpoint was reached while this hook was registered.
        reached: Mutex<HashMap<&'static str, usize>>,
    }

    type Registry = Mutex<HashMap<PathBuf, Vec<Arc<Registered>>>>;

    fn registry() -> MutexGuard<'static, HashMap<PathBuf, Vec<Arc<Registered>>>> {
        static REGISTRY: OnceLock<Registry> = OnceLock::new();
        REGISTRY
            .get_or_init(Registry::default)
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The registration of a closure; dropping it removes the closure.
    pub struct Hook {
        slot: PathBuf,
        inner: Arc<Registered>,
    }

    impl Hook {
        /// How often the checkpoint `at` was reached for this slot since the closure was
        /// registered. A test asserts it, so that a checkpoint that moved away from the code it
        /// stands for fails the test and does not just stop testing anything.
        pub fn reached(&self, at: &str) -> usize {
            self.inner
                .reached
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(at)
                .copied()
                .unwrap_or(0)
        }
    }

    impl Drop for Hook {
        fn drop(&mut self) {
            let mut registry = registry();
            if let Some(hooks) = registry.get_mut(&self.slot) {
                hooks.retain(|h| !Arc::ptr_eq(h, &self.inner));
                if hooks.is_empty() {
                    registry.remove(&self.slot);
                }
            }
        }
    }

    /// Call `f(at, n)` at every checkpoint reached for the slot at `slot`, where `at` names the
    /// checkpoint and `n` counts how often `at` was reached before (from 0). The closure decides
    /// where to act: `|at, n| if at == "write" && n == 0 { ... }`.
    ///
    /// Keep the returned [`Hook`] for as long as the closure is to be called.
    pub fn on_checkpoint(slot: &Path, f: impl FnMut(&'static str, usize) + Send + 'static) -> Hook {
        let inner = Arc::new(Registered {
            callback: Mutex::new(Box::new(f)),
            reached: Mutex::new(HashMap::new()),
        });
        registry()
            .entry(slot.to_owned())
            .or_default()
            .push(Arc::clone(&inner));
        Hook {
            slot: slot.to_owned(),
            inner,
        }
    }

    pub(super) fn fire(slot: &Path, at: &'static str) {
        // The closures run outside the registry's lock: they touch the file system, and a
        // checkpoint may be reached from inside one. That checkpoint is counted, and the closure
        // already running on this thread is not called again for it (its own lock would never be
        // free); another thread waits for it.
        let hooks: Vec<Arc<Registered>> = registry().get(slot).cloned().unwrap_or_default();
        for hook in hooks {
            let n = {
                let mut reached = hook.reached.lock().unwrap_or_else(PoisonError::into_inner);
                let count = reached.entry(at).or_insert(0);
                let before = *count;
                *count += 1;
                before
            };
            let id = Arc::as_ptr(&hook);
            if RUNNING.with(|running| running.borrow().contains(&id)) {
                continue;
            }
            RUNNING.with(|running| running.borrow_mut().push(id));
            let mut callback = hook.callback.lock().unwrap_or_else(PoisonError::into_inner);
            (*callback)(at, n);
            drop(callback);
            RUNNING.with(|running| running.borrow_mut().retain(|r| *r != id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{checkpoint, on_checkpoint};
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    /// A closure that reaches a checkpoint of its own slot returns: the inner checkpoint is
    /// counted and does not call the closure again (before, it waited for its own lock forever).
    #[test]
    fn a_checkpoint_reached_from_inside_its_own_closure_returns() {
        let slot = Path::new("/race-test/reentrant");
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let hook = on_checkpoint(slot, move |at, _| {
            seen.fetch_add(1, Ordering::SeqCst);
            if at == "outer" {
                checkpoint(Path::new("/race-test/reentrant"), "inner");
            }
        });
        let (done, wait) = mpsc::channel();
        std::thread::spawn(move || {
            checkpoint(Path::new("/race-test/reentrant"), "outer");
            let _ = done.send(());
        });
        wait.recv_timeout(Duration::from_secs(10))
            .expect("a checkpoint reached from inside its own closure must not wait forever");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!((hook.reached("outer"), hook.reached("inner")), (1, 1));
    }
}
