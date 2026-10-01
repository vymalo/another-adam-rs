//! `adam-exec`, the script that runs in the container, executed here by `sh` (it needs a `/proc`
//! and nothing else). The script is the one the crate embeds and mounts.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/adam-exec.sh");

fn adam_exec(state: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg(SCRIPT).args(args).env("ADAM_EXEC_DIR", state);
    cmd
}

fn output(cmd: &mut Command) -> (i32, String, String) {
    let out = cmd.output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        && std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s.contains(") Z "))
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(10), "{what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_script_is_the_one_the_crate_embeds_and_passes_sh_n() {
    let text = std::fs::read_to_string(SCRIPT).unwrap();
    assert!(text.starts_with("#!/bin/sh\n"));
    let (code, _, err) = output(Command::new("sh").arg("-n").arg(SCRIPT));
    assert_eq!(code, 0, "{err}");
}

#[test]
fn run_enters_the_directory_and_becomes_the_program_with_every_word_unmarked() {
    let state = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (code, out, _) = output(&mut adam_exec(
        state.path(),
        &[
            "run",
            "id-1",
            cwd.path().to_str().unwrap(),
            ":sh",
            ":-c",
            ":printf '%s|' \"$PWD\" \"$@\"",
            ":sh",
            ":--version",
            ":a b",
            ":",
            ":-x",
        ],
    ));
    assert_eq!(code, 0);
    assert_eq!(out, format!("{}|--version|a b||-x|", cwd.path().display()));
    // The exit code is the program's.
    let (code, _, _) = output(&mut adam_exec(
        state.path(),
        &["run", "id-2", "/", ":sh", ":-c", ":exit 7"],
    ));
    assert_eq!(code, 7);
    // The process is recorded with its start time.
    let pid_file = std::fs::read_to_string(state.path().join("id-2.pid")).unwrap();
    let fields: Vec<&str> = pid_file.split_whitespace().collect();
    assert_eq!(fields.len(), 2, "{pid_file:?}");
    assert!(fields[0].parse::<u32>().is_ok() && fields[1].parse::<u64>().is_ok());
}

#[test]
fn shell_runs_a_command_line_in_a_login_shell_and_keeps_its_exit_code() {
    let state = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (code, out, _) = output(&mut adam_exec(
        state.path(),
        &[
            "shell",
            "id-1",
            cwd.path().to_str().unwrap(),
            ":pwd -P; echo \"a  b\" | tr a-z A-Z; exit 3",
        ],
    ));
    assert_eq!(code, 3);
    let canonical = std::fs::canonicalize(cwd.path()).unwrap();
    assert_eq!(out, format!("{}\nA  B\n", canonical.display()));
}

#[test]
fn a_missing_directory_or_a_bad_id_stops_with_a_reason() {
    let state = tempfile::tempdir().unwrap();
    let (code, _, err) = output(&mut adam_exec(
        state.path(),
        &["run", "id-1", "/no/such/dir", ":true"],
    ));
    assert_eq!(code, 126);
    assert!(err.contains("cannot enter /no/such/dir"), "{err}");
    for bad in ["../x", "a b", "", "a/b"] {
        let (code, _, err) = output(&mut adam_exec(state.path(), &["run", bad, "/", ":true"]));
        assert_eq!(code, 2, "{bad:?}");
        assert!(err.contains("bad id"), "{err}");
    }
    let (code, _, err) = output(&mut adam_exec(state.path(), &["nonsense"]));
    assert_eq!(code, 2);
    assert!(err.contains("usage"), "{err}");
}

/// Start `sh -c <script>` through `adam-exec run` in the background; returns its pid file's pid.
/// With `leader`, in a process group of its own.
fn start(state: &Path, id: &str, script: &str, leader: bool) -> (std::process::Child, u32) {
    use std::os::unix::process::CommandExt;
    let mut cmd = adam_exec(
        state,
        &["run", id, "/", ":sh", ":-c", &format!(":{script}")],
    );
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if leader {
        cmd.process_group(0);
    }
    let child = cmd.spawn().unwrap();
    let pid_file: PathBuf = state.join(format!("{id}.pid"));
    wait_until("the pid file", || {
        std::fs::read_to_string(&pid_file).is_ok_and(|t| t.split_whitespace().count() == 2)
    });
    let pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    (child, pid)
}

/// The pids of the processes whose command line holds `needle`.
fn pids_of(needle: &str) -> Vec<u32> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline"))
            && String::from_utf8_lossy(&cmdline)
                .replace('\0', " ")
                .contains(needle)
            && pid != std::process::id()
            && alive(pid)
        {
            found.push(pid);
        }
    }
    found
}

#[test]
fn kill_stops_the_process_and_everything_it_started() {
    let state = tempfile::tempdir().unwrap();
    // A distinctive sleep that a child and a grandchild also run.
    let marker = format!("sleep 31{}", std::process::id() % 1000);
    let script = format!("{marker} & ({marker}; true) & wait");
    let (mut child, pid) = start(state.path(), "kill-1", &script, false);
    wait_until("the sleeps to start", || pids_of(&marker).len() >= 3);
    let (code, _, err) = output(&mut adam_exec(state.path(), &["kill", "kill-1"]));
    assert_eq!(code, 0, "{err}");
    child.wait().unwrap();
    wait_until("everything to be gone", || {
        pids_of(&marker).is_empty() && !alive(pid)
    });
    assert!(
        !state.path().join("kill-1.pid").exists(),
        "the record is removed"
    );
    // Again: nothing to do, and no error.
    assert_eq!(
        output(&mut adam_exec(state.path(), &["kill", "kill-1"])).0,
        0
    );
    assert_eq!(
        output(&mut adam_exec(state.path(), &["kill", "never-started"])).0,
        0
    );
}

/// A background process that lost its parent is not a descendant any more: it is found by its
/// process group when the command leads one.
#[test]
fn kill_finds_what_lost_its_parent_through_the_process_group_of_a_leader() {
    let state = tempfile::tempdir().unwrap();
    let marker = format!("sleep 32{}", std::process::id() % 1000);
    let script = format!("({marker} &) ; {marker}");
    let (mut child, pid) = start(state.path(), "kill-2", &script, true);
    wait_until("the sleeps to start", || pids_of(&marker).len() >= 3);
    assert_eq!(
        output(&mut adam_exec(state.path(), &["kill", "kill-2"])).0,
        0
    );
    child.wait().unwrap();
    wait_until("the orphan to be gone too", || {
        pids_of(&marker).is_empty() && !alive(pid)
    });
}

#[test]
fn kill_never_touches_a_process_that_only_has_the_same_pid() {
    let state = tempfile::tempdir().unwrap();
    // A record whose pid is alive but whose start time is not the process's: a reused pid.
    let mut bystander = Command::new("sleep")
        .arg("30")
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    std::fs::write(
        state.path().join("stale-1.pid"),
        format!("{} 1\n", bystander.id()),
    )
    .unwrap();
    assert_eq!(
        output(&mut adam_exec(state.path(), &["kill", "stale-1"])).0,
        0
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        alive(bystander.id()),
        "the process that is not the command was left alone"
    );
    assert!(!state.path().join("stale-1.pid").exists());
    bystander.kill().unwrap();
    bystander.wait().unwrap();
    // A record that is not numbers kills nothing either.
    std::fs::write(state.path().join("junk-1.pid"), "not-a-pid x\n").unwrap();
    assert_eq!(
        output(&mut adam_exec(state.path(), &["kill", "junk-1"])).0,
        0
    );
}

#[test]
fn chown_validates_its_arguments_and_leaves_a_missing_directory_alone() {
    let state = tempfile::tempdir().unwrap();
    for args in [
        vec!["chown", "abc", "1", "/tmp"],
        vec!["chown", "", "1", "/tmp"],
        vec!["chown", "1", "x", "/tmp"],
        vec!["chown", "1000", "1000", "relative"],
        vec!["chown", "1000", "1000"],
    ] {
        let (code, _, _) = output(&mut adam_exec(state.path(), &args));
        assert_eq!(code, 2, "{args:?}");
    }
    assert_eq!(
        output(&mut adam_exec(
            state.path(),
            &["chown", "1000", "1000", "/no/such/directory"]
        ))
        .0,
        0
    );
}

/// The ids are the ones outside the container; the script finds the ones inside in the id map and
/// hands `chown` those. `chown` here is a stub that says what it was asked.
#[test]
fn chown_maps_the_ids_outside_the_container_to_the_ones_inside_it() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("chown"), "#!/bin/sh\necho \"chown $*\"\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join("chown"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    let tree = dir.path().join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let run = |proc_dir: &Path| {
        let mut cmd = adam_exec(
            dir.path(),
            &["chown", "10001", "10001", tree.to_str().unwrap()],
        );
        cmd.env("PATH", &path).env("ADAM_EXEC_PROC", proc_dir);
        output(&mut cmd)
    };
    let proc_with = |uid_map: &str, gid_map: &str| {
        let p = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(p.path().join("self")).unwrap();
        std::fs::write(p.path().join("self/uid_map"), uid_map).unwrap();
        std::fs::write(p.path().join("self/gid_map"), gid_map).unwrap();
        p
    };
    let flags = "-hR";
    // keep-id: the user's own number is mapped to itself, root and the rest to the subuids.
    let keep = proc_with(
        "         0     100000      10001\n     10001      10001          1\n     10002     110002      55535\n",
        "         0     100000      10001\n     10001      10001          1\n     10002     110002      55535\n",
    );
    assert_eq!(
        run(keep.path()),
        (
            0,
            format!("chown {flags} 10001:10001 {}\n", tree.display()),
            String::new()
        )
    );
    // Without it: root is the user outside, and a subuid range follows.
    let root_mapped = proc_with("0 10001 1\n1 100000 65536\n", "0 10001 1\n1 100000 65536\n");
    assert_eq!(
        run(root_mapped.path()),
        (
            0,
            format!("chown {flags} 0:0 {}\n", tree.display()),
            String::new()
        )
    );
    // No userns at all: the identity map.
    let identity = proc_with("0 0 4294967295\n", "0 0 4294967295\n");
    assert_eq!(
        run(identity.path()),
        (
            0,
            format!("chown {flags} 10001:10001 {}\n", tree.display()),
            String::new()
        )
    );
    // The coder's ids are not in the container's map: nothing to give back, and no error.
    let unmapped = proc_with("0 100000 65536\n", "0 100000 65536\n");
    assert_eq!(run(unmapped.path()), (0, String::new(), String::new()));
}
