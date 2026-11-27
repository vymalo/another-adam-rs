//! `adam-agent --version` and `--help`: answered before any configuration is read, so they run with
//! an empty environment and no database (the Homebrew formula's `test do` block runs `--version`).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::process::Command;

fn run(arg: &str) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_adam-agent"))
        .arg(arg)
        .env_clear()
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

#[test]
fn version_prints_the_name_and_the_cards_version() {
    for flag in ["--version", "-V"] {
        let (ok, stdout, stderr) = run(flag);
        assert!(ok, "{flag} failed: {stderr}");
        let line = stdout.trim_end();
        let version = line.strip_prefix("adam-agent ").expect("name first");
        assert!(
            version.starts_with(env!("CARGO_PKG_VERSION")),
            "version {version}"
        );
        assert!(!line.contains('\n'), "one line: {line}");
        assert!(stderr.is_empty(), "no log line: {stderr}");
    }
}

#[test]
fn help_names_what_a_laptop_needs() {
    for flag in ["--help", "-h"] {
        let (ok, stdout, _) = run(flag);
        assert!(ok);
        for needle in [
            "ADAM_AGENT_DIR",
            "DATABASE_URL",
            "sqlite://",
            "A2A_BEARER_TOKENS",
            "PUBLIC_URL",
            "MODEL_BASE_URL",
        ] {
            assert!(stdout.contains(needle), "{flag} lacks {needle}: {stdout}");
        }
    }
}
