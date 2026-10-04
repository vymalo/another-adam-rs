//! Keeping the coder's secrets from its own children, beyond the names it hides from them.
//!
//! The checks, commands and OpenCode of a run are processes of the coder's user, and a process of a
//! user can read `/proc/<pid>/environ` of another of the same user. [`make_non_dumpable`] closes that
//! door for the coder itself (Linux).

/// Make the process **non-dumpable** (`prctl(PR_SET_DUMPABLE, 0)`, through `rustix`'s safe call, so the
/// crate stays `forbid(unsafe_code)`): `/proc/<pid>/environ`, `/proc/<pid>/mem` and ptrace of this process
/// are then refused to every process of the same user that lacks `CAP_SYS_PTRACE`, which is what the
/// children of a run are (the chart drops every capability). Without it, `cat /proc/<coder pid>/environ`
/// in a repository's test script would give them every secret of this process (`GITHUB_TOKEN`,
/// `MODEL_API_KEY`, the keys an MCP server reads) that hiding names cannot reach. The setting is
/// inherited by `fork` and reset by `exec`, so the children themselves stay ordinary processes. A
/// failure is logged and is not fatal: the process is then as it was. Other systems: nothing to do.
///
/// Returns whether the setting was made.
pub fn make_non_dumpable() -> bool {
    #[cfg(target_os = "linux")]
    {
        match rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
        {
            Ok(()) => {
                tracing::info!(
                    "the process is non-dumpable: its environment is not readable from /proc by its children"
                );
                true
            }
            Err(error) => {
                tracing::warn!(%error, "cannot make the process non-dumpable");
                false
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn a_child_of_the_same_user_cannot_read_the_environment_of_a_non_dumpable_process() {
        if rustix::process::geteuid().is_root() {
            // `CAP_SYS_PTRACE` reads it anyway: the guarantee is for the users a run's children are.
            return;
        }
        let read = || {
            std::process::Command::new("sh")
                .args(["-c", "cat /proc/$PPID/environ >/dev/null 2>&1"])
                .status()
                .unwrap()
                .success()
        };
        assert!(read(), "a same-user child reads it before");
        assert!(make_non_dumpable());
        assert!(!read(), "and cannot after");
    }
}
