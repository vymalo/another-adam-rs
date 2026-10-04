//! The environment of a `git` the coder starts itself.
//!
//! A worktree is written by code that is not ours (a repository's checks, a command, OpenCode), and
//! git runs what a repository's configuration names: a `filter.<name>.clean` command written to
//! `.git/config` and a committed `.gitattributes` make the coder's own next `git add -A` run that
//! command, in the coder's process environment. So every `git` the coder starts begins from an
//! **empty** environment and gets back only what it needs ([`GIT_INHERITED_ENV`]), plus what the caller
//! sets on purpose afterwards (the push token travels in `GIT_CONFIG_VALUE_0`, for the one command
//! that pushes). The secrets of the coder (`GITHUB_TOKEN`, `MODEL_API_KEY`, the keys an MCP server
//! reads) are not in that list and never reach a repository's filter.

use tokio::process::Command;

/// The variables of this process that a `git` inherits: where to find programs and a home, the
/// locale and temporary directory, the certificate and proxy settings a network needs (a proxy URL
/// can carry a password: a deployment that sets one accepts that git sees it), and the
/// operator's `GIT_CONFIG_GLOBAL`. Everything else of this process's environment is dropped.
pub const GIT_INHERITED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LANGUAGE",
    "TMPDIR",
    "TMP",
    "TEMP",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "CURL_CA_BUNDLE",
    "GIT_SSL_CAINFO",
    "GIT_SSL_CAPATH",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "GIT_CONFIG_GLOBAL",
];

/// Start `cmd` from an empty environment and give it back [`GIT_INHERITED_ENV`] as this process has
/// it. Call it **before** setting any variable of your own on `cmd`.
pub fn confine_git_env(cmd: &mut Command) {
    cmd.env_clear();
    for name in GIT_INHERITED_ENV {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// A bare-bones repository whose configuration makes `git add` run a filter that writes the
    /// environment it sees to `out`: what repository code can do to the coder's own git.
    fn repo_with_filter(dir: &Path, out: &Path) {
        let run = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        run(&["init", "-q", "."]);
        run(&[
            "config",
            "filter.spy.clean",
            &format!("sh -c 'env > {}; cat'", out.display()),
        ]);
        std::fs::write(dir.join(".gitattributes"), "*.txt filter=spy\n").unwrap();
        std::fs::write(dir.join("a.txt"), "content\n").unwrap();
    }

    /// `CARGO_PKG_NAME` is set by cargo in every test process: it stands for a secret of the coder.
    const SECRET_NAME: &str = "CARGO_PKG_NAME";

    #[tokio::test]
    async fn a_filter_a_repository_configures_does_not_see_the_coders_environment() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("seen-by-filter");
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        repo_with_filter(&repo, &out);

        // Control: an ordinary spawn inherits the environment, and the filter sees the variable, so
        // what follows proves something.
        let status = Command::new("git")
            .args(["add", "-A"])
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .await
            .unwrap();
        assert!(status.success());
        let seen = std::fs::read_to_string(&out).unwrap();
        assert!(seen.contains(&format!("{SECRET_NAME}=")), "{seen}");
        std::fs::remove_file(&out).unwrap();
        std::fs::remove_file(repo.join(".git/index")).unwrap();

        // Confined: the same `git add -A` runs the same filter, which sees the inherited names and
        // not the secret.
        let mut cmd = Command::new("git");
        confine_git_env(&mut cmd);
        let status = cmd
            .args(["add", "-A"])
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .await
            .unwrap();
        assert!(status.success());
        let seen = std::fs::read_to_string(&out).unwrap();
        assert!(!seen.contains(SECRET_NAME), "{seen}");
        assert!(seen.contains("PATH="), "what git needs is kept: {seen}");
    }
}
