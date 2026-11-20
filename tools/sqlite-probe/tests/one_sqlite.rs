//! ADR 0053 decision 2: `adam-store-sqlite` and CrateStack's embedded SQLite runtime must resolve
//! to **one** `rusqlite` and **one** `libsqlite3-sys`, or an application that links both has two
//! `sqlite3` copies.
#![allow(clippy::unwrap_used, clippy::expect_used)] // a probe asserts by unwrapping

use std::collections::BTreeSet;
use std::process::Command;

#[test]
fn adam_store_sqlite_and_cratestack_sqlite_share_one_sqlite3() {
    let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = Command::new(cargo)
        .args([
            "tree",
            "--manifest-path",
            manifest,
            "--locked",
            "--features",
            "probe",
            "--prefix",
            "none",
            "-e",
            "normal",
        ])
        .output()
        .expect("cargo tree runs");
    assert!(
        output.status.success(),
        "cargo tree failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8(output.stdout).expect("cargo tree writes UTF-8");

    // A resolution that lost either side would pass the count assertions vacuously.
    assert!(
        tree.contains("adam-store-sqlite v"),
        "the probe must resolve the adapter:\n{tree}"
    );
    assert!(
        tree.contains("cratestack-rusqlite v"),
        "the probe must resolve cratestack's SQLite runtime:\n{tree}"
    );

    let rusqlite: BTreeSet<&str> = tree
        .lines()
        .filter(|line| line.starts_with("rusqlite v"))
        .filter_map(|line| line.split_whitespace().nth(1))
        .collect();
    let sqlite: BTreeSet<&str> = tree
        .lines()
        .filter(|line| line.starts_with("libsqlite3-sys v"))
        .filter_map(|line| line.split_whitespace().nth(1))
        .collect();
    assert_eq!(
        rusqlite.len(),
        1,
        "one rusqlite, found {rusqlite:?}:\n{tree}"
    );
    assert_eq!(
        sqlite.len(),
        1,
        "one libsqlite3-sys, found {sqlite:?}:\n{tree}"
    );
}
