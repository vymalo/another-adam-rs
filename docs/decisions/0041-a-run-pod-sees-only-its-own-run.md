# 0041. A run pod sees only its own run

Status: **Proposed** (2026-10-18). Built in the same change (`adam-env-kubernetes`, the chart's pod template and
admission policy, the tests below); the decisions the owner must confirm are at the end, and what is unverified is
marked. Amends [ADR 0019](0019-a-runs-processes-in-a-pod-of-their-own.md): decision 2 (what the code fills in),
decision 3 (the files), decision 7 (the admission policy), OD-K1 and the consequence "It is not isolation between runs".
Builds on [ADR 0008](0008-a-workspace-holds-several-repositories.md) (slots) and
[ADR 0010](0010-a-run-works-in-its-repositorys-devcontainer.md) (a path means the same in every environment), and
qualifies [ADR 0040](0040-a-shared-package-cache-at-cache.md) (the cache's trust domain).

## Context

ADR 0019 gave each active run a pod for its commands and said plainly that this was not isolation between runs:
the pod mounts the coder's whole workspace volume at `/work`, so that one owner's runs could share it. The coder is now
to be offered to every user, so a command of one person's run must not see another person's run. What a run pod could
reach on the volume, *verified 2026-10-18 by reading `crates/adam-workspace/src/workspace.rs` (the layout) and
`deploy/coder/templates/_runpods.tpl` at `b819770`*:

| On the volume | A command could |
|---|---|
| `workspaces/<run>/<slot>/`, every run's worktrees and scratch projects | read every other run's files (source of repositories the person never named) |
| `meta/<run>/`, the slots' metadata | read their repository URLs and branches |
| `git/<host>/<owner>/<repo>.git`, a bare mirror shared by every run of that repository | read every repository the coder ever mirrored, **and write the mirror**: its `config`, `hooks/`, `info/attributes`, refs and objects |

The write is worse than the read. The coder's own `git` runs in every run's worktree of that mirror. It starts from an
empty environment, never runs hooks or an fsmonitor, and removes some configuration keys (`url.*`, `remote.*`, `http.*`,
`credential.*`, `core.sshCommand`, ...) before a credentialed command (`Inner::sanitize_mirror_config`); it does not stop a
`filter.*` or `diff.*` command in the mirror's configuration, a rewritten ref (another person's unpushed `agent/*`
branch) or a poisoned object. A run that writes the mirror reaches the coder and every other run of that repository.

`KubeSession::prepare` refuses a working directory outside `workspaces/<run>`, which is a check on the caller's
request, not on what a command reads. The admission policy let any pod the coder made reference **any key** of the
coder's Secret, or mount it, although the template uses `MODEL_API_KEY` only; the Secret also holds `GITHUB_TOKEN`,
`A2A_BEARER_TOKENS` and the MCP tokens (`externalsecret.yaml`).

## Decision

1. **A run pod mounts the workspace volume only through `subPath` mounts, at the same paths as in the coder.**

   | Mount (path in the pod and in the coder) | Mode | For |
   |---|---|---|
   | `<root>/workspaces/<run>` | read-write | the run's slots |
   | `<root>/git/<host>/<owner>/<repo>.git`, for each worktree slot | **read-only** | the objects, refs and configuration a worktree's `.git` file points into |
   | `<root>/git/<host>/<owner>/<repo>.git/worktrees/<name>`, for each worktree slot | read-write, mounted on top | the worktree's `HEAD` and `index`, which `git status` refreshes |
   | `.../worktrees/<name>/HEAD`, `commondir`, `gitdir` and `logs` | **read-only**, mounted on top of that | the branch the coder's `reset` moves (decision 11), what its `git` follows out of the directory (decision 7), what ties it to its slot, and the reflog it appends to |

   Nothing of `meta/`, of another run, or of another repository's mirror is in the pod. `<name>` is `<run>+<slot>` (decision 9)
   and **comes from the coder's metadata, never from the slot's `.git` file** (decision 6). A bind mount of a file or of a
   directory cannot be written, renamed over, replaced by a link or removed from the pod (*verified*, decision 10), though the
   directory around it is writable and its files belong to the very uid that tries. A
   scratch slot is a repository inside the run's directory and needs nothing more. The legacy worktree of a run
   (`<root>/worktrees/<run>`) is outside the run's directory, where `prepare` already refuses a working directory, and
   gets no mount. A worktree whose directory in the mirror is not known (decision 6), or whose `HEAD`, `commondir`, `gitdir` or
   `logs` is not what git wrote (a link, a pipe, a missing file; a missing `logs` is made by the coder), gets no git mounts and a
   warning in the log: less access is safe, and the next `ensure` tries again.
2. **The template marks the workspace by mounting its volume once, whole, at the workspace root; the code replaces that
   mount.** This amends decision 2 of ADR 0019: the code fills in the pod's name, namespace, labels and annotations,
   **and the mounts of that one volume**; every other part of the template stays as the deployment wrote it
   (`PodTemplate::render`). The mount is the run container's mount whose path is the workspace root
   (`WORKSPACE_ROOT`, or the worker's folder under it) or the closest directory above it. The coder refuses to start
   (exit 78, as for every template mistake) when there is none; when it has a `subPath`, is read-only, names a volume
   the pod does not have, or when the volume is mounted by any other container or a second time. Where the template
   mounts the volume is where the `subPath`s start.
3. **The mounts of a pod are fixed when it is made, so a repository added later needs a pod made again.** The pod's
   annotation `adam.vymalo.com/workspace-mounts` records its mounts (`rw <path>` / `ro <path>`, one per line; the lines of
   a worktree's directory in the mirror end in ` @<generation>`, below). Each
   `ensure` computes the mounts the run's workspace needs now and compares: a mount the pod lacks (a worktree slot added
   since; a pod of the earlier version, which has no annotation and mounts everything) is a reason to make the pod
   again, and a mount the pod has and the run no longer needs is not. **A bind mount follows a directory and not its name**:
   a slot whose directory was lost and added again has the same name for its directory in the mirror (decision 9) and a new
   directory under it, and the pod's mounts, whose lines would be the same, keep the deleted one, so git in the pod would see
   a `HEAD` and an index that are not the coder's. So the coder records a *generation* in the slot's metadata (`admin_gen`, new
   each time it makes the directory: the time in nanoseconds and a count, in hex), `Worktree::git_dir_generation` says it, and
   the lines of the directory and of its four read-only entries carry it: a changed generation is a mount the pod lacks. A
   slot made before the generation was recorded has none, and its lines are what they were. `ensure` asks the pod with `adam-exec active`:
   with no command running it deletes the pod and makes another (the files are on the volume, ADR 0019 decision 5);
   with one running, or when the pod cannot be asked, it answers `EnvError::Unavailable`, which the caller waits out as
   it waits for a slot (ADR 0019 decision 6, up to `RUN_POD_WAIT_SECS`). A command prepared a moment before is killed
   with the pod and fails; the model can run it again. Only a worktree needs a mount of its own: a scratch project, or a
   directory added inside the run's directory, is already visible.
4. **The run's directory is made by the coder before its pod is.** A `subPath` that does not exist is made by the
   kubelet, as root.
5. **The admission policy allows exactly one Secret reference**: one `secretKeyRef` of the coder's Secret, by name
   (`coder.secretName`) and by the key `MODEL_API_KEY` (one `define`, used by the template and the policy). It refuses a
   Secret volume, `envFrom` of a Secret, and any other `secretKeyRef`, so that no run pod holds a credential but the model
   key. Against a compromised coder this buys little (it holds those secrets itself); it is the check that a mistake
   in the template, or a change later, cannot put `GITHUB_TOKEN` in a pod a command runs in. ADR 0038 removes even this key
   later and is not built here.
6. **The worktree's directory in the mirror is the coder's record, not a file the run can write.** The `.git` file of a
   slot is written by git, and then by whatever runs in the slot: a command that writes `gitdir: <mirror>/worktrees/<B's
   name>` into it would have the coder, at the next pod made again, mount run B's directory (its `HEAD`, `index`,
   `commondir`) read-write into this run's pod, and tamper with B's run. So the coder records the name in the slot's own
   metadata (`meta/<run>/<dir>.json`, field `admin`, which no pod mounts) when it makes the worktree: it lists the mirror's
   `worktrees/` before and after `git worktree add` (under the mirror's lock; a pod cannot write that directory) and takes
   the one new entry whose `gitdir` names the new worktree. The metadata version stays 2: the field is optional, old metadata
   reads as before and an older coder ignores the field. **A record is believed only while it is still the run's**: each time
   a slot is opened the recorded name must carry the run (decision 9), be a directory of its own (not a link, not a file), and
   its `gitdir` must name the slot. A record that fails this (the directory was pruned and the name given to another run's
   worktree, which is what a moved slot made possible before decision 9) leaves the worktree with no git directory. For a slot
   made before the record, the directory is the **one** `worktrees/*/gitdir` that names the slot's `.git` among the directories
   that carry no run (git's own names, from before decision 9) or this run, and is then recorded if it carries the run; none or
   several is no answer (a run can add a candidate by writing its own `gitdir`, so the worst it can do is make the answer none),
   and then the slot gets no git mounts and the coder's own git refuses it (`WorkspaceError::Corrupt`, "cannot tell which
   directory"). `Worktree::git_dir` and `Worktree::git_pin` say it.
7. **Every `git` the coder runs on a slot is pinned to that record.** `GIT_DIR=<mirror>/worktrees/<name>`,
   `GIT_COMMON_DIR=<mirror>` and `GIT_WORK_TREE=<slot>` are set on each (`GitPin`; the workspace's worktree commands,
   `commit_all`, `push`, `publish`, `diff_stat`, the coder's `run_command` snapshot and restore, `run_checks`, the tree ids,
   `git apply` of `apply_patch`), so the slot's `.git`, and the `commondir` and `gitdir` of its directory, which a run's commands write,
   steer nothing. A scratch project's pin is its own
   `.git` for both directories and requires that it be a real directory (a file or link a command put there to send git into
   another run's worktree or a mirror is refused). What pinning does not cover, and what is done about each:

   | What git reads in a slot | Result (git 2.43.0) | Done |
   |---|---|---|
   | `.git` file of the slot | ignored once `GIT_DIR` is set (*verified*: a clean filter planted in the repository it names did not run, and ran for an unpinned `git add -A`) | pinned |
   | `commondir` of the worktree's directory | `GIT_COMMON_DIR` wins for the configuration, hooks, attributes and objects (*verified*: the planted filter did not run), **but git's reference store still reads the file** (`refs/remotes/origin/main` was "not a valid object name" with `commondir` pointing at another directory) | a **read-only mount** in a run pod, so a run cannot write it; the coder only **checks** it (`GitPin::check`: a regular file, opened without following a link, that says `../..`) and refuses git on the worktree otherwise. It does not put the file back: the first version did, by opening the file a run can replace by a link, and wrote through the link to any file the coder can write (*verified* by a test that failed before) |
   | `HEAD` of the worktree's directory | data, but `git commit` follows it: a run that points it at another run's branch has the coder commit its files there (*verified* with a test that failed before); and `reset` follows it too (decision 11) | the commit is made with plumbing onto `refs/heads/<the run's branch>`, whatever `HEAD` says, with the old tip as the compare-and-swap (`commit_all_in`); the coder's snapshot and restore take the branch from the metadata (decision 11); and a **read-only mount** in a run pod, so a run cannot write it |
   | `config.worktree` of the worktree's directory | read **only** with `core.repositoryformatversion` 1 **and** `extensions.worktreeConfig` true in the common configuration (*verified*: with both, a filter in it ran; with format 0 it did not) | the coder makes format-0 mirrors and never turns the extension on (a test pins both); `extensions.worktreeConfig` is among the keys the workspace removes from the mirror's configuration before a credentialed command |
   | `logs/HEAD` of the worktree's directory | git **appends** to the file a link there names (*verified* with git 2.43.0: a line `<old> <new> <identity> <time> <message>` was added to a file that is not a log), which would corrupt any file the coder can write, the mirror's `config` among them | `logs` is a read-only mount in a run pod, and `GitPin::check` refuses a worktree whose `logs` or `logs/HEAD` is a link or a pipe (a check, not a seal, where nothing is mounted) |
   | `index` | data: no command named in it runs (`core.fsmonitor` is forced off); the coder copies it (`diff_stat`, the tree ids) | read as a regular file and copied into a private directory of the coder (decision 10) |
   | `HEAD`, `ORIG_HEAD`, `info/sparse-checkout` and the like | data: no command named in them runs (`core.fsmonitor` is forced off; hooks are off); git writes `HEAD` and `ORIG_HEAD` by lock file and rename, which replaces a link | `HEAD` and `index` must be regular files at the check |
   | `info/attributes`, `info/exclude`, `hooks/`, `packed-refs`, `objects/` | the common directory's (`git rev-parse --git-path`), read-only in a pod | none |
   | the working tree's own `.gitattributes` | selects a driver, whose command comes from the configuration, which is the mirror's | none |

   Local mode and Podman are unchanged and work with the pin: it names the same directories git would have found. They
   are not protected by it: the commands share the coder's container, or bind the mirror read-write.
8. **Git in a run pod reads, and refreshes its index.** *Verified 2026-10-18 with git 2.43.0 and real bind mounts*
   (`deploy/coder/tests/git-in-run-pod.sh`, which builds the pod's view in a private mount namespace and runs git as uid
   10001 in it): `status` (also after the working tree changed), `log`, `diff`, `describe`, `rev-parse`, `grep`,
   `show`, `worktree list` and `checkout -- <file>` work. Whatever writes an object, a ref or the
   configuration of the shared mirror fails: `add`, `commit`, `stash`, `reset --hard`, `tag`, `branch`, `config --local`, a
   `fetch` that brings objects, and a script that installs hooks (`husky install`). **So does moving `HEAD`** (`switch --detach`,
   `checkout <commit>`): it appends to `logs/HEAD`, which is read-only (decision 1), and git says "unable to append to ...
   Read-only file system". Before decision 1 had `logs` it worked; the coder's `run_command` undoes and refuses such a change
   anyway. `HEAD` itself is a read-only file there (decision 11): `blame`, `show`, `for-each-ref` and `branch --show-current` work,
   and nothing git does for what a check needs writes `HEAD`. That is the point, and the coder does the
   committing (ADR 0010: git stays in the coder). The mirror's `worktrees/` still lists every run's directory of that
   repository, with their `HEAD`, index and log (below).

9. **A worktree's directory in the mirror is called `<run>+<slot>`, and nothing is pruned.** Git names it after the last
   component of the path, so every run's directory for the same repository starts from the same name, and `git worktree
   prune`, which the coder ran before each new worktree, removes any directory whose `gitdir` names a path that is gone. A
   command that renames its slot away makes its own directory stale (*verified*: a test failed before); the next run's worktree
   was then given the name again, and the first run, putting its slot back, found another run's directory under the name the
   coder had recorded, which the coder would have pinned its git to and mounted read-write into the first run's pod. Now a
   worktree is made in `<root>/staging/<run>+<slot>` (no pod mounts it) and moved to its slot with `git worktree move`, which
   keeps the name (*verified*, git 2.43.0); `+` is in no run id and no slot name, so no other run's directory begins with
   `<run>+`. The coder runs **no `git worktree prune`**: it removes what an earlier try at the same slot left by name (and a
   directory that carries no run and names the same slot), removes a run's directories by name when the run goes (and, when its
   metadata cannot be read, every `<run>+...` directory of every mirror), and does not judge another run's directory stale,
   whatever its `gitdir` says. The base checkout (`add_base_checkout`) is called `<run>+<slot>+base` and made the same way.
    The directories above the destination (the run's directory, `.adam-base`) are the run's to write, and `git worktree move`
    follows a link there and, when something is at the destination, moves the tree **into** it. So the way to the destination
    is looked at before the move and again after it (every component a plain directory, from the run's directory down), and the
    new directory in the mirror must name the destination. When it does not, the coder removes the directory in the mirror, the
    staging copy and the tree where git put it (`<destination>/<name>`) **if the way to it is plain**, and returns an error that
    says where it went; a tree behind a link is left (a removal would follow the link to wherever the run points it by now: it is
    inert, its directory in the mirror being gone), and so is a directory that appeared at the destination, which is not the
    coder's. A swap in the milliseconds between the first look and the move is detected and undone this way, not prevented.
10. **What the coder opens in a directory a run can write, it opens without following a link.** The run's pod writes
    its run's directory and the worktree's directory in the mirror, and the coder later meets whatever it left there, with the
    rights of the coder, which holds the whole volume and the GitHub App's key (`crates/adam-workspace/src/safe_fs.rs`):
    * a file it reads (`commondir`, `gitdir`, the slot's `.git`, the index) is opened `O_NOFOLLOW | O_NONBLOCK` and must be a
      regular file of an accepted size, so a link leads nowhere and a pipe does not make the coder wait (a test hung before);
    * a copy it makes (`diff_stat`, the coder's tree ids copy the index) is made in a `0700` directory of its own in the process's
      temporary directory, off the volume, with a name nobody chose and `O_EXCL`; `git` works on it through `GIT_INDEX_FILE`. The
      first version copied to `index.adam-diff-<pid>-<n>` beside the index: a link planted at a name it could predict was written
      through, and an `index` that was a link copied any readable file (the App's key) into the directory the run reads;
    * a file it writes there (the slot's `.git`, put back by `run_command`'s restore) is written beside it and renamed over it;
    * a slot is a **plain directory** (`lstat`) that is a direct child of the run's directory, and so is the directory `git` is
      pinned to (`GitPin::check`): a link in place of a slot, to another run's slot, is not a slot, is not listed, and is refused
      by every `git`; the coder's file tools (`confine`) look at the root without following a link before they canonicalise it. The
      first version took such a link for a slot (*verified*: another run's `.env` was committed on this run's branch);
    * a scratch project's directory and its `.git` are looked at without following a link before `create_dir_all` and `git init`
      (`Scratch::init`, on every `add_scratch`), and are refused when they are a link or a file: the first version asked `try_exists`,
      which follows a link, and made the repository where the link led. The repository's `info/exclude`, which a snapshot reads
      and a restore writes, is read as a regular file, and written by replacing it; `info` must be a directory itself, or the
      state is not known and nothing is run (`run_command`'s `RepoState`);
    * `git worktree remove` resolves a link and deletes the worktree it leads to (*verified*, git 2.43.0) when the path is not
      the one a worktree is registered at (a run that writes its own `gitdir`, as local mode, Podman and a pod made before the
      mounts can, arranges that; when it is, git checks that the `.git` file points back and refuses), so it is asked only about a
      path that is plain all the way down from the run's directory; a link is removed, not followed. The same holds for the base
      checkout and its directory `.adam-base`.

11. **`HEAD` of the worktree's directory is read-only in a run pod, and the coder's restore takes the run's branch from its
    metadata.** `HEAD` is a file the run's pod could write (the directory is read-write for `index`), and `reset` and `commit`
    act on the branch it names. The coder's `run_command` and `run` snapshot the worktree before a command and put it back when
    the command changed what it must not; the first version took the branch to put `HEAD` on, and to reset, from `HEAD` itself:
    when a command (or an earlier one) had written another run's `agent/*` branch there, the restore reset **that** branch to the
    commit the snapshot had seen (*verified*: a test failed before; the other run's branch was moved back to the older commit).
    Now:
    * in a run pod `HEAD` is a **read-only file mount** like `commondir` (decision 1). Git in the pod does not write it: it cannot move a
      ref (the mirror is read-only), and the commands that move `HEAD` fail anyway on the reflog (decision 8). *Verified 2026-10-18*
      with `git-in-run-pod.sh` (status, log, diff, describe, blame, show, for-each-ref, `branch --show-current`, `checkout -- <file>`
      and a refreshed index work; a write, append, rename over, removal or link at `HEAD`, `symbolic-ref HEAD` and `update-ref
      --no-deref HEAD` fail; the script fails when `HEAD` is left out of the mounts). The coder does not rewrite it either: it puts
      `HEAD` back only when it differs (`point_head`), because a file replaced by a new one is a file the pod's mount no longer
      follows (the problem of decision 3);
    * the snapshot of a repository's worktree (`Snapshot::of_slot`) takes the branch from `Worktree::local_branch`, never from
      `HEAD`; it records whether `HEAD` was on `refs/heads/<that>`; when it was not, `run_command` and `run` **run nothing**
      and say so ("HEAD of this worktree is on ..., not on the run's own branch ...: nothing was run ... tell the person"), and a
      restore from such a snapshot writes nothing and returns `false`. A restore of a worktree whose `HEAD` was on its branch
      before the command puts `HEAD` back on that branch and resets that branch only;
    * `reset` follows `HEAD`, so a `HEAD` that is flipped between the restore's `symbolic-ref` and its `reset` would still be
      followed. In a pod `HEAD` cannot be flipped; in local mode and Podman, which nothing here protects (Known limits), it can.

## Known limits, not built

* **A mirror is shared by everyone who works on a repository.** Two people on one repository can read each other's
  unpushed `agent/*` branches (refs and objects) through the read-only mirror. They can also read **every sibling run's
  directory in `worktrees/`**, which is in the mirror: its `gitdir` (the other run's id, now in the directory's name too, and the path
  of its slot in the volume), its `HEAD` (the branch it is on), its `index` (the paths and blob ids of what it has staged, not the
  contents of files that were never committed) and its `logs`. The names of the files and the ids of the runs are what leaks; the
  contents of an uncommitted file are not in the mirror. Left open: it is the shared-mirror trade-off above, closed by a mirror
  for each person (open decision 3), which needs the person's identity, which a later step brings.
* **The package cache is writable by every run** (ADR 0040), and other users' builds read it. One run can write into
  what another person's `npm ci` or `cargo build` takes: a cross-tenant integrity risk. ADR 0040 called the volume "one
  trust domain: every run on it is the owner's"; that holds only while the runs are. Left as it is (open decision 2).
* **An embedded repository in the working tree runs its own configuration in the coder's git.** Pinning decides which
  repository the coder's git works on, not what is inside its working tree. A directory with a `.git` of its own (a
  nested repository, a submodule checkout; `modules/` of the worktree's directory holds the gitdirs of submodules) that the
  index lists as a gitlink is entered by `git status`, `git add -A`, `git diff` and `git commit`, with that
  repository's configuration: a `filter.*.clean` there ran (*verified* by hand with git 2.43.0 and the spy of the tests; a
  gitlink is made by a run's `git add sub` in its pod, which writes only the index, or by the coder's own `git add -A`
  the first time it sees the nested repository). `-c diff.ignoreSubmodules=all` stops `status` and `diff` but not `add -A`
  and hides a changed gitlink from `diff --cached --quiet`, so it is not applied. The coder's git starts from an empty
  environment and without hooks or a monitor; what it cannot be kept from is a command that runs as the coder's user in the
  coder's container, which holds the GitHub App key. Closing it needs the coder's git to run apart from the coder's secrets
  (another user, or a sandbox), or to refuse a worktree with an embedded repository (racy, since the run writes the index
  and the directory at any time). Open decision 6.
* **A run's commands can still write the index of their own directory in the mirror.** The index is the content of the next
  commit, which is the run's own work. (`HEAD`, `commondir`, `gitdir` and `logs` cannot be written from a pod, decisions 1 and 11;
  in local mode and Podman `HEAD` can, and the coder's commit and restore do not follow it.)
* **A swap between a check and the use that follows it is not closed for a slot's directory.** `GitPin::check` and the reads of
  decision 10 look at a path and then git (or the coder) uses it; in a run pod the files that matter (`commondir`, `gitdir`,
  `logs`) are mounts and cannot be swapped, but a slot's directory can be renamed by the run's commands at any time, and a link put
  there in the milliseconds between the check and git's read would be followed once. Closing it makes each slot a mount point (a
  new slot would then need a pod made again, which decision 3 already does for worktrees but not for scratch projects), or has the
  coder hold a descriptor of the directory for the length of a command. Not built.
* **A scratch project is a repository the run controls entirely.** Its `.git` is a directory in the run's directory, with its own
  `config` (a `filter.*.clean` or a `core.fsmonitor` there runs in the coder's `git add -A`, decision 7's open decision 6), its
  own `logs` (the link-append of decision 7 reaches any file through `.git/logs/HEAD`), and its own hooks (off). The pin requires
  `.git` to be a plain directory without a `commondir`; it cannot make what is inside it the coder's. The coder's own reads and
  writes of its files are made without following a link (decision 10: `info/exclude`), but `git` itself reads `info/exclude`,
  `info/attributes` and `packed-refs` there with a plain `open`: a link or a pipe at one of them would be followed or waited on by the
  coder's `git add -A` (*unverified*: not tried). The remedy is the one for the
  embedded repository below: the coder's git apart from the coder's secrets.
* **Slots made before decision 9 keep git's own names** (`worktrees/<repo>`, no run in them). They are found by the one `gitdir`
  that names the slot among the directories that carry no run, as before; a run that owns another such directory can make that
  answer ambiguous (none), and, if the slot's own directory is gone, name its own as the only candidate. They age out with their runs.
* **Local mode and Podman** share the coder's container or bind the mirror read-write: none of this protects them.
* **Git in a base checkout does not work in a run pod.** `add_base_checkout` makes a second worktree for the check on the
  base (`.adam-base/<slot>` in the run's directory); its directory in the mirror is made after the pod and is not mounted, so
  `git` there says it is not a repository. The commands that do not use git run as before.
* **A repository's devcontainer on Podman (ADR 0010) binds the mirror read-write.** Not touched here.
* The model key is in every run pod's environment until ADR 0038 is built.

## Alternatives rejected

* **A read-write mirror with a policy in the coder** (refuse a command that writes it): a command is a shell; the mount
  is the only thing that holds.
* **Mounting the slot's `.git` read-only** to pin the chain from the worktree to the mirror: a command can
  rename the slot's directory and make another with a `.git` of its own, which the coder's git then reads; the slot
  directory itself would have to be a mount, and then a rename by the coder (`adopt_scratch`) breaks it. The coder's own git is pinned
  instead (decision 7). What the pin does not cover and a mount can, `commondir`, `gitdir` and `logs` of the worktree's directory,
  is mounted read-only (decision 1).
* **A private copy of the whole git directory for the coder's git** (a `HEAD` the coder writes, the index copied in and written
  back, `commondir` made by the coder): it removes every file of the run's directory from what git reads but the index, and
  would need `run_command`'s snapshot and restore, which act on the real `HEAD`, rewritten. Not built; the mounts and the
  checks above are what there is.
* **`git worktree prune` scoped by a name**: prune has no scope; deleting by name does the same job.
* **An `emptyDir` over `worktrees/` to hide other runs' directories:** three more mounts for each repository, for a
  trust domain (the shared refs) that stays open until a mirror per person.
* **Mounting single files of the mirror** (`config`, `packed-refs`): git replaces them by rename, so the pod would keep
  the old inode.
* **A clone for each run in the pod:** the files are on the volume so that a pod can be thrown away (ADR 0019 decision 5).
* **Hot-adding the mount to the running pod:** needs mount propagation and a privileged container.

## Facts, and how they are known

* The git behaviour in decision 6 and that git names a second worktree of a mirror `r1`: *verified 2026-10-18*, git 2.43.0,
  `git-in-run-pod.sh` (CI runs it in the `run-pods` job).
* The behaviours in decisions 6 and 7 (a rewritten `.git`, `commondir` and `HEAD`, a forged `gitdir`, an ambiguous or missing
  back-pointer, `config.worktree` with and without the extension, a scratch `.git` replaced by a file, an embedded
  repository): *verified 2026-10-18* with git 2.43.0 by the tests of `crates/adam-workspace/tests/workspace.rs`, each of
  which fails without the change it tests (the pin, the explicit commit), and by hand for the embedded repository.
* The behaviours of decisions 1, 9 and 10, *verified 2026-10-18* with git 2.43.0: a file bind-mounted read-only is not writable, appendable,
  renamable over, removable or replaceable by a link by the uid that owns it, nor a read-only `logs` directory (`git-in-run-pod.sh`); git
  appends a reflog line through a link at `logs/HEAD`; `git worktree remove --force <link>` deletes the worktree the link leads to (by
  hand, in the scratchpad); `git worktree move` keeps the directory's name; `git worktree prune` removes the directory of a worktree
  whose path is gone; a worktree is named after the last component of its path; a copy of the index into the directory the run
  writes, at a predictable name, was written through a planted link, and a linked `index` was copied; a linked slot was committed
  on another run's branch; and a pipe where the coder reads a file hung it. Each of those in `crates/adam-workspace/tests/workspace.rs`
  failed when run against the sources of the first version (`dc7e4d5`); the tests of `bin/adam-coder` (`gitcli.rs`, `files.rs`) were
  written with the change and are the same cases on the coder's side.
* The behaviours of decisions 3, 9 to 11 added by the third version, *verified 2026-10-18* with git 2.43.0 and each failing against
  the sources of `5aa41e5`: a snapshot taken with `HEAD` on another run's branch restored by resetting that branch
  (`bin/adam-coder` `head_tests`, and `run_command` and `run` refusing, `tests/tools.rs`); a slot made again under the same name
  gives the pod's annotation another line, and the pod is made again (`fake.rs`, with the generation of `tests/workspace.rs`); `git
  worktree move` into a directory that appeared at the destination puts the tree at `<destination>/<name>` and leaves the directory
  in the mirror naming it (`placement_tests`); a scratch directory or `.git` that is a link was made into a repository where it led
  (`tests/workspace.rs`); `info/exclude` as a link was read and written through (`exclude_tests`); a read-only bind mount of `HEAD`
  cannot be written, renamed over, removed or linked, and git's read-only commands work with it (`git-in-run-pod.sh`).
* The policy's expressions: *verified 2026-10-18* with an independent CEL implementation (cel-python) on thirteen sample
  pods (the intended pod admitted; another key, another Secret's key, a Secret volume, an init container's reference,
  `envFrom` and a projected Secret refused; a ConfigMap reference admitted); *unverified* against the API server's own
  type checker and runtime until the `run-pods` job has run (it also refuses three more changes there).
* That a container runtime mounts a read-only `subPath` directory with a read-write `subPath` directory inside it, that
  the kubelet makes a missing `subPath` with the volume root's mode, and the whole isolation scenario
  (`a_run_pod_sees_only_its_own_run` in `tests/cluster.rs`, seeded by `kind-run-pods.sh`): *unverified* until CI runs
  the `run-pods` job; no container daemon exists where this was written. containerd and CRI-O sort mounts so that a
  parent comes first (*unverified*, from memory).

## Decisions for the owner to confirm

1. **Git in a run pod cannot write.** The mirror is read-only, so a command that runs `git commit`, `git add` of a new file,
   `git stash`, `git tag`, `git config --local` or a hook installer fails in a run pod. The coder commits and pushes
   (`commit_and_push`), and OpenCode, while it stays (ADR 0038), will see the refusal. The alternative, a writable object
   store, lets a run plant objects every run reads.
2. **The shared package cache** (ADR 0040) stays as decided and stays a cross-tenant integrity risk. Open: **a cache for each
   person, or read-only in run pods** (a writer outside them, or the pull-through proxy of ADR 0040). Until then, share it
   only among runs that trust each other.
3. **A mirror for each person**, instead of for each repository, closes the shared refs and worktree directories. It needs
   the person's identity in the coder.
4. **A new repository waits for a running command** (or makes the pod again when none runs) instead of ending the command.
5. **Exactly one Secret key in a run pod, `MODEL_API_KEY`**, until ADR 0038 removes it.
7. **Moving `HEAD` in a run pod fails** (`git switch --detach`, `git checkout <commit>`), because `logs` is read-only; the coder's
   `run_command` and `run` undo such a change anyway. The alternative is a writable `logs`, which leaves a link at `logs/HEAD` able to
   make the coder's git append to any file it can write; the coder could instead never write the worktree's reflog (its own
   `update-ref` through the mirror, its restores without `reset`), which is a rewrite of `run_command`'s restore and does not hold
   for a future command.
8. **`HEAD` is read-only in a run pod, and a worktree whose `HEAD` is not on its branch runs no command.** Sealing `HEAD` closes
   the reset of another run's branch for pods; the metadata rule closes it for the coder's own state and covers local mode and
   Podman up to the race in decision 11. The cost: a run that wanted to move `HEAD` by hand in its pod could not (it already
   could not, decision 7 of this list), and a worktree that is found with `HEAD` elsewhere (only a command that wrote the file can
   have done it) is not usable by `run_command` and `run` until the person looks at it, instead of being put right by the coder. The
   alternative is to put `HEAD` back on the run's branch and go on, which leaves the question of what else that command changed.
   A pod made before this change has no `HEAD` mount in its annotation and is made again once, when no command runs in it.
6. **The embedded-repository vector above.** Run the coder's git apart from its secrets (a user of its own or a sandbox), or
   accept it while every run is a person who may be trusted with the coder's container. (The pin and the explicit commit
   are decided and built; whether the Podman devcontainer should get a read-only mirror too is still open.)

## Consequences

* A command of a run sees its own directory, the mirror of each repository the run works on, and nothing of any other
  run, of `meta/` or of any other repository's mirror. It can no longer write what the coder's git reads in other runs.
* A run that adds a repository while a command runs waits for it to end; one that adds a repository while nothing runs
  pays the making of a pod (scheduling, and a pull on a node that lacks the image) once more.
* A pod made by the earlier version is made again at the next `ensure` (it has no annotation, or lacks the `HEAD` mount), after its
  command ends; and so is the pod of a run whose slot was made again under the same name (the generation).
* The coder's `git` on a slot is pinned and `Worktree::commit_all` commits with plumbing onto the run's own branch; this
  holds in local mode and with Podman too (the pin names the directories git would have found). A slot made before the
  directory in the mirror was recorded is resolved from the mirror at its next listing, or refused if that is ambiguous.
  `Worktree` gains `git_dir`, `git_dir_generation` and `git_pin`, `Slot` and `Scratch` gain `git_pin`, and the new public type is
  `GitPin`. `WorktreeDirs` gains the field `generation` and `Mount` the method `generation` (a struct literal of `WorktreeDirs`
  needs the field).
* `PodTemplate::parse` and `from_file` take the workspace root; `render` takes the mounts; `MOUNTS_ANNOTATION`,
  `WorkspaceMounts`, `Mount` and `WorktreeDirs` are new. The chart's template, its policy and the goldens change; no value
  and no environment variable does, and `RUN_ENVIRONMENT=local` and `runPods.enabled: false` are as they were.

## Amended

* **2026-10-18, after a security review of the first version (#117).** The coder's file operations on paths a run can write (decision
  10); a slot that is a link is not a slot (decision 10); the directories of the mirror are `<run>+<slot>`, made in staging and moved,
  never pruned, and a record is believed only while it is still the run's (decisions 6 and 9); `commondir`, `gitdir` and `logs` are
  read-only mounts and `GitPin::check` only checks (decisions 1 and 7), where the first version rewrote `commondir` through the link
  the run had put there; the sibling `worktrees/*` exposure is stated (Known limits); moving `HEAD` in a pod no longer works (decision
  8, owner decision 7). The residual races and the scratch project are in Known limits.
* **2026-10-18, after the second review.** `HEAD` of the worktree's directory is a read-only mount, and the coder's snapshot and
  restore take the run's branch from the metadata and run nothing when `HEAD` is elsewhere (decision 11); a scratch project's
  directory and `.git` are looked at without following a link before they are made, and its `info/exclude` is read and written like
  `commondir` (decision 10); `git worktree move` is verified afterwards and undone when it went somewhere else (decision 9); the
  mounts of a pod carry the generation of the worktree's directory, so a slot made again has the pod made again (decision 3).
