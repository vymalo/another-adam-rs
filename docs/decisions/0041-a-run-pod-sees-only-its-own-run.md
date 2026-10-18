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
   | `<root>/git/<host>/<owner>/<repo>.git/worktrees/<name>`, for each worktree slot | read-write, mounted on top | the worktree's `HEAD`, `index` and logs, which `git status` refreshes |

   Nothing of `meta/`, of another run, or of another repository's mirror is in the pod. `<name>` is whatever git picked
   (`r`, then `r1`, ...) and **comes from the coder's metadata, never from the slot's `.git` file** (decision 6). A
   scratch slot is a repository inside the run's directory and needs nothing more. The legacy worktree of a run
   (`<root>/worktrees/<run>`) is outside the run's directory, where `prepare` already refuses a working directory, and
   gets no mount. A worktree whose directory in the mirror is not known (decision 6) gets no git mounts and a warning in
   the log: less access is safe, and the next `ensure` tries again.
2. **The template marks the workspace by mounting its volume once, whole, at the workspace root; the code replaces that
   mount.** This amends decision 2 of ADR 0019: the code fills in the pod's name, namespace, labels and annotations,
   **and the mounts of that one volume**; every other part of the template stays as the deployment wrote it
   (`PodTemplate::render`). The mount is the run container's mount whose path is the workspace root
   (`WORKSPACE_ROOT`, or the worker's folder under it) or the closest directory above it. The coder refuses to start
   (exit 78, as for every template mistake) when there is none; when it has a `subPath`, is read-only, names a volume
   the pod does not have, or when the volume is mounted by any other container or a second time. Where the template
   mounts the volume is where the `subPath`s start.
3. **The mounts of a pod are fixed when it is made, so a repository added later needs a pod made again.** The pod's
   annotation `adam.vymalo.com/workspace-mounts` records its mounts (`rw <path>` / `ro <path>`, one per line). Each
   `ensure` computes the mounts the run's workspace needs now and compares: a mount the pod lacks (a worktree slot added
   since; a pod of the earlier version, which has no annotation and mounts everything) is a reason to make the pod
   again, and a mount the pod has and the run no longer needs is not. `ensure` asks the pod with `adam-exec active`:
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
   the one new entry whose `gitdir` names the new slot. The metadata version stays 2: the field is optional, old metadata
   reads as before and an older coder ignores the field. For a slot made before the record, the directory is the **one**
   `worktrees/*/gitdir` that names the slot's `.git`, and is then recorded; none or several is no answer (a run can add a
   candidate by writing its own `gitdir`, so the worst it can do is make the answer none), and then the slot gets no git
   mounts and the coder's own git refuses it (`WorkspaceError::Corrupt`, "cannot tell which directory"). `Worktree::git_dir`
   and `Worktree::git_pin` say it.
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
   | `commondir` of the worktree's directory | `GIT_COMMON_DIR` wins for the configuration, hooks, attributes and objects (*verified*: the planted filter did not run), **but git's reference store still reads the file** (`refs/remotes/origin/main` was "not a valid object name" with `commondir` pointing at another directory) | put back to `../..` before each command (`GitPin::check`) |
   | `HEAD` of the worktree's directory | data, but `git commit` follows it: a run that points it at another run's branch has the coder commit its files there (*verified* with a test that failed before) | the commit is made with plumbing onto `refs/heads/<the run's branch>`, whatever `HEAD` says, with the old tip as the compare-and-swap (`commit_all_in`) |
   | `config.worktree` of the worktree's directory | read **only** with `core.repositoryformatversion` 1 **and** `extensions.worktreeConfig` true in the common configuration (*verified*: with both, a filter in it ran; with format 0 it did not) | the coder makes format-0 mirrors and never turns the extension on (a test pins both); `extensions.worktreeConfig` is among the keys the workspace removes from the mirror's configuration before a credentialed command |
   | `index`, `logs/`, `info/sparse-checkout`, `ORIG_HEAD` and the like | data: no command named in them runs (`core.fsmonitor` is forced off; hooks are off) | none |
   | `info/attributes`, `info/exclude`, `hooks/`, `packed-refs`, `objects/` | the common directory's (`git rev-parse --git-path`), read-only in a pod | none |
   | the working tree's own `.gitattributes` | selects a driver, whose command comes from the configuration, which is the mirror's | none |

   Local mode and Podman are unchanged and work with the pin: it names the same directories git would have found. They
   are not protected by it: the commands share the coder's container, or bind the mirror read-write.
8. **Git in a run pod reads, and refreshes its index.** *Verified 2026-10-18 with git 2.43.0 and real bind mounts*
   (`deploy/coder/tests/git-in-run-pod.sh`, which builds the pod's view in a private mount namespace and runs git as uid
   10001 in it): `status` (also after the working tree changed), `log`, `diff`, `describe`, `rev-parse`, `grep`,
   `show`, `worktree list`, `checkout -- <file>` and moving `HEAD` (`switch --detach`) work. Whatever writes an object, a ref or the
   configuration of the shared mirror fails: `add`, `commit`, `stash`, `reset --hard`, `tag`, `branch`, `config --local`, a
   `fetch` that brings objects, and a script that installs hooks (`husky install`). That is the point, and the coder does the
   committing (ADR 0010: git stays in the coder). The mirror's `worktrees/` still lists every run's directory of that
   repository, with their `HEAD`, index and log (below).

## Known limits, not built

* **A mirror is shared by everyone who works on a repository.** Two people on one repository can read each other's
  unpushed `agent/*` branches (refs and objects) and the directories of each other's worktrees in `worktrees/`, through the
  read-only mirror. A mirror for each person needs the person's identity, which a later step brings.
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
* **A run's commands can still write `HEAD`, the index and the logs of their own directory in the mirror.** `HEAD` decides
  what `status`, `diff` and `describe` say and what the model is told (the coder's own commit does not follow it, decision 7);
  the index is the content of the next commit, which is the run's own work.
* **Git in a base checkout does not work in a run pod.** `add_base_checkout` makes a second worktree for the check on the
  base (`.adam-base/<slot>` in the run's directory); its directory in the mirror is made after the pod and is not mounted, so
  `git` there says it is not a repository. The commands that do not use git run as before.
* **A repository's devcontainer on Podman (ADR 0010) binds the mirror read-write.** Not touched here.
* The model key is in every run pod's environment until ADR 0038 is built.

## Alternatives rejected

* **A read-write mirror with a policy in the coder** (refuse a command that writes it): a command is a shell; the mount
  is the only thing that holds.
* **Mounting `.git`, `commondir` and `gitdir` read-only** to pin the chain from the worktree to the mirror: a command can
  rename the slot's directory and make another with a `.git` of its own, which the coder's git then reads; the slot
  directory itself would have to be a mount, and then a rename by the coder (`adopt_scratch`) breaks it. The coder's own git is pinned instead (decision 7).
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
6. **The embedded-repository vector above.** Run the coder's git apart from its secrets (a user of its own or a sandbox), or
   accept it while every run is a person who may be trusted with the coder's container. (The pin and the explicit commit
   are decided and built; whether the Podman devcontainer should get a read-only mirror too is still open.)

## Consequences

* A command of a run sees its own directory, the mirror of each repository the run works on, and nothing of any other
  run, of `meta/` or of any other repository's mirror. It can no longer write what the coder's git reads in other runs.
* A run that adds a repository while a command runs waits for it to end; one that adds a repository while nothing runs
  pays the making of a pod (scheduling, and a pull on a node that lacks the image) once more.
* A pod made by the earlier version is made again at the next `ensure` (it has no annotation), after its command ends.
* The coder's `git` on a slot is pinned and `Worktree::commit_all` commits with plumbing onto the run's own branch; this
  holds in local mode and with Podman too (the pin names the directories git would have found). A slot made before the
  directory in the mirror was recorded is resolved from the mirror at its next listing, or refused if that is ambiguous.
  `Worktree` gains `git_dir` and `git_pin`, `Slot` and `Scratch` gain `git_pin`, and the new public type is `GitPin`.
* `PodTemplate::parse` and `from_file` take the workspace root; `render` takes the mounts; `MOUNTS_ANNOTATION`,
  `WorkspaceMounts`, `Mount` and `WorktreeDirs` are new. The chart's template, its policy and the goldens change; no value
  and no environment variable does, and `RUN_ENVIRONMENT=local` and `runPods.enabled: false` are as they were.

## Amended

Nothing yet.
