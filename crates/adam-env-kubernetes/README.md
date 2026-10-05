# adam-env-kubernetes

A run's processes in a Kubernetes pod of its own: the cluster-backed
[`Environment`](../adam-workspace/README.md#where-a-runs-processes-run-the-environment-port) of `adam-workspace`.
The decision, the facts it rests on, and the lifecycle of a run pod are
[ADR 0019](../../docs/decisions/0019-a-runs-processes-in-a-pod-of-their-own.md).

## Where it sits

An **implementation** of the `Environment` port, swapped at build time (a binary composes it, no plugin), next to
[`adam-devcontainer`](../adam-devcontainer/README.md) (a Podman service). It depends on `adam-workspace` for the port and on
`kube` for the cluster: it speaks to the API server itself and runs no command to do it. The coder composes it when
`RUN_ENVIRONMENT=kubernetes` (see [its README](../../bin/adam-coder/README.md#a-pod-of-its-own-for-each-run)); the default is the
coder's own container, so nothing changes for a deployment that does not opt in.

A run's commands (`run_command`, `run_checks`, OpenCode and everything OpenCode starts) go through the run's `EnvSession`: the
coder `prepare`s a command and spawns it. The files, the paths and all git work stay in the coder, which holds the credentials;
**a path means the same in the pod as in the coder**, because the pod mounts the coder's workspace volume at the same path.
**No GitHub credential, database URL or bearer token is ever in a run pod.**

## API at a glance

| Item | What |
|---|---|
| `KubeEnvironment` | the `Environment`: `connect(Settings, PodTemplate)` (the cluster this process runs in, or `KUBECONFIG`'s), `new(Client, Settings, PodTemplate)`, `reap_idle()` (one sweep of idle pods), `reap_loop()` and `spawn_reaper()` (the sweep, for as long as the runtime lives), `pod_name(run)`. `rebuild(run, _)` deletes the pod and answers `true`; the next `ensure` makes another |
| `PodTemplate` | the deployment's pod: `from_file(path, container)` or `parse(text, source, container)` (a Pod, or a bare PodSpec, in YAML or JSON), checked at startup; `render(run, name, Placement)` fills in the name, namespace, labels and annotations and nothing else; `image()` |
| `Settings` | the namespace, the release's instance label, the worker, the container the commands run in (`run`), the exec client program, where `adam-exec` and the tools are in the pod, the time to be ready, the idle timeout, the sweep interval and the environment `adam-kube-exec` is started with; `Settings::new(namespace, instance, worker)` has the defaults |
| `Invocation` | the command line of `adam-kube-exec`: `to_args()`, `parse(args)`, `remote_argv()` (the command run in the pod); `Mode::{Run, Shell}`, `UsageError` |
| `names` | `pod_name(run)` (`adam-run-<12 hex of the sha256 of the run id>`), `run_hash`, `selector(instance)` and the label and annotation keys (`MANAGED_BY_LABEL`, `INSTANCE_LABEL`, `RUN_LABEL`, `RUN_ID_ANNOTATION`, `WORKER_ANNOTATION`) |
| `install_crypto_provider()` | names rustls' process default provider (aws-lc-rs) unless one is named: `kube` builds its TLS configuration with the default, and rustls panics rather than guess when a binary's tree enables two providers, which the coder's does (sqlx's rustls enables `ring`). `KubeEnvironment::connect` and `adam-kube-exec` call it; a caller that makes its own `kube::Client` calls it first. Idempotent. Proved in a tree that has both by a test in the coder (`tests/environment.rs`) |
| `exec` | what `pods/exec` gives: `stream(..)` joins a command's stdin, stdout and stderr to the caller's, `exit_status(&Status)` reads the exit code, `stdin_is_null()` |

```rust,ignore
use adam_env_kubernetes::{KubeEnvironment, PodTemplate, Settings};
use adam_workspace::Environment;

let template = PodTemplate::from_file("/etc/adam/run-pod/pod.yaml".as_ref(), "run")?;
let settings = Settings::new("coder", "coder", "coder-0");
let environment = KubeEnvironment::connect(settings, template).await?;
environment.spawn_reaper();
// environment.ensure(&run_workspace, &progress).await? gives the session of the run.
```

## What `ensure` does

It looks for the pod `adam-run-<hash>` of the run and makes it when it is not there, from the template, with the labels
`app.kubernetes.io/managed-by=adam-coder`, `app.kubernetes.io/instance=<release>` and `adam.vymalo.com/run=<hash>` and the
annotations `adam.vymalo.com/run-id` (the full id) and `adam.vymalo.com/worker`. A name that is another run's is refused. It
then waits until the pod is `Running` and `Ready`, polling, and reports one step (`run-pod`) while it waits and when it is
done; a pod that is there and ready is reused with **no step**, so a command does not announce itself. It is single-flight per
run in a process (a lock), and across workers the pod's name is the lock: a create that answers "already exists" goes on with
the pod that is there. A dropped call leaves nothing a later call cannot use.

| What the cluster says | The result |
|---|---|
| the pod is ready | the session |
| 403 `exceeded quota` on create | `EnvError::Unavailable` (class `Transient`): the coder waits and tries again |
| 403 (RBAC), 422 or 400 (the admission policy, a bad object) on create | `EnvError::Refused` (class `Invalid`): the deployment is wrong |
| the API does not answer, a 5xx | `EnvError::Unavailable` |
| not ready in `ready_timeout`, and no node has room (`PodScheduled=False`) | the pod is deleted; `EnvError::Unavailable` |
| not ready in `ready_timeout`, and a container's image is refused or its configuration is wrong (`ErrImagePull`, `ImagePullBackOff`, `InvalidImageName`, `CreateContainerConfigError`, ...) | the pod is deleted; `EnvError::Build` with the reason |
| not ready in `ready_timeout`, only slow | `EnvError::Timeout { phase: "start" }`; the pod stays, the next `ensure` waits for it again |
| the pod ended (`Failed`, `Succeeded`: evicted) | it is deleted and made again |

## The template

The pod is the deployment's, not the code's: the image, the resources, the priority class, the security context, the volumes,
the variables from Secrets and the node affinity are in the file. The code adds only the name, the namespace, the labels and
the annotations. The file is read and checked at startup, so that a template that cannot work stops the coder and not a run:
it may not set `metadata.name`, `generateName` or owner references, nor the coder's labels and annotations; it must have the
container the commands run in (`RUN_POD_CONTAINER`, `run`); and it may not ask for what the chart's admission policy
refuses (`hostNetwork`, `hostPID`, `hostIPC`, a `hostPath` volume, a privileged container, privilege escalation, a pinned
`nodeName`, or an `automountServiceAccountToken` that is not `false`). The chart renders it (`runPods` in
[`deploy/coder`](../../deploy/coder/README.md)); `src/template.rs` has the shape in its tests.

The commands run in the container named `run`, which must mount `/opt/adam/bin` read-only from an `emptyDir` that an init
container filled from the coder's image (`adam-exec`, and `opencode` for `tool_path("opencode")`), the workspace volume at the
same path as in the coder, and have `MODEL_API_KEY` from the deployment's Secret when the gateway needs a key
(`Settings::model_key`, which `secret_ref("model-key")` follows).

## Processes

`prepare(spec)` returns a `PreparedCommand` whose program is `adam-kube-exec` (`Settings::exec_client`) with the arguments
`Invocation::to_args` makes: `--namespace`, `--pod`, `--container`, `--adam-exec`, one `--env NAME=VALUE` for each variable of
the spec that is not hidden (and `GIT_CONFIG_*` for `safe.directory`), one `--unset NAME` for each name the spec hides, the
mode (`run` for an argv, `shell` for a command line), the exec id, the working directory and the words of the command, each
with the `:` in front that `adam-exec` removes. Every word is its own argument all the way to `adam-exec`; nothing is quoted
and no shell reads it, except in `shell` mode. The program starts with an empty environment (`env_clear`) and only the
variables of `Settings::client_env` (`KUBERNETES_SERVICE_HOST`, `KUBERNETES_SERVICE_PORT`, `KUBECONFIG`, `HOME`, `PATH` of the
coder's, those that are set), so none of the coder's secrets reaches the client. A `cwd` outside the run's workspace, an
empty argv, a non-UTF-8 word and a variable name that is not one are `EnvError::Refused`.

`kill(exec)` runs `adam-exec kill <id>` in the pod (the process, its descendants and its process group); it never fails, what
it cannot do is logged. Closing the exec client does not stop a command in the pod, so the caller kills the client **and**
calls this.

## `adam-kube-exec`

The binary of this crate (`src/bin/adam-kube-exec.rs`, shipped in the coder's image) that a `PreparedCommand` names: it runs
`env [-u NAME]... [NAME=VALUE]... /opt/adam/bin/adam-exec run|shell <id> <cwd> :<word>...` in the run container with `pods/exec`,
copies its stdin to the command (unless its own stdin is `/dev/null`, in which case the command has none: a `cat` ends at once, as
it does locally), copies stdout and stderr back as they come, and **exits with the command's exit code**.

```text
adam-kube-exec --namespace NS --pod POD [--container NAME] [--adam-exec PATH]
               [--env NAME=VALUE]... [--unset NAME]... run|shell ID CWD :WORD...
```

It reaches the cluster as the coder's ServiceAccount does (`KUBERNETES_SERVICE_HOST` and the mounted token), or through
`KUBECONFIG`. The options come first and everything after the mode is positional, so a word such as `--version` is never
taken for an option; a command line that is wrong is refused before anything runs (`Invocation::parse`, tested for every
refusal). Exit codes: the command's own (a command killed by a signal has 128 plus the signal, as the container runtime
reports it); **64** a wrong command line; **69** the cluster could not run it or the connection ended before the command did;
129, 130, 143 when the client itself is hung up, interrupted or terminated (`BSD sysexits.h`' values, *unverified*, from memory).
Closing the client does not stop the command in the pod: the coder calls `kill`, which runs `adam-exec kill`.

Closing stdin ends the command's stdin through `v5.channel.k8s.io` stream close, which `kube` negotiates (*verified* by
reading `kube-client` 4.2.0); a cluster that does not speak it closes the whole stream instead and loses output. Kubernetes
1.30 or later is assumed (*unverified* here, the owner's cluster).

## Teardown, and the idle timeout

`release(run)` deletes the pod (with a two second grace period: the quota counts a pod until it is gone) and is idempotent: a
run with no pod is not an error. `held_runs()` lists the pods of this release by label
(`app.kubernetes.io/managed-by=adam-coder,app.kubernetes.io/instance=<release>`) and returns the run ids from the annotation,
which is what the coder's janitor sweeps at startup and every `WORKSPACE_SWEEP_SECS`.

A pod that **no command used for `Settings::idle`** (900 seconds) is deleted by `reap_idle`, which `spawn_reaper` runs every
`Settings::reap_every`, and the next `ensure` makes another: the files are on the volume. The clock is this process's (every
`ensure`, `prepare` and `kill` of the run restarts it; a pod this process has not seen yet starts its clock when it is first
seen), the sweep only looks at pods whose `adam.vymalo.com/worker` annotation is this worker, and a pod is asked, with
`adam-exec active`, whether a command is still alive in it (a long build, OpenCode) before it is deleted: a busy pod restarts
its clock. A pod that cannot be asked stays for the next sweep. `None` never sweeps.

## Errors

`EnvError` of `adam-workspace`, with its classes (`Unavailable`, `Lost` and `Timeout` are `Transient`; `Config`, `Refused` and
`Build` are `Invalid`; `Io` is `Internal`). A message names this layer and the cluster's own words (cut to 400 characters, on
one line); none carries a secret, and the API server never echoes one in a refusal.

## Tests

* **Unit tests, no cluster.** The environment is tested over a fake API server (`src/fake.rs`): a `tower-test` mock service
  behind a real `kube::Client`, which keeps pods in memory and answers `GET`, `POST`, `DELETE` and list with the answers a test
  turns on (a quota, an admission refusal, a pod that takes some polls to be ready, one that never schedules, an image that
  cannot be pulled). Commands in a pod are another seam (`PodExec`) with a fake. They cover `ensure` (the pod, its labels, its
  steps, idempotency, concurrent calls, a create that races), the refusals above, `release`, `held_runs` (this release's pods
  only), the idle sweep (idle, busy, cannot be asked, another worker's, off), `prepare` and the quoting of the words, `kill`,
  and `tool_path` and `secret_ref`.
* **Tests that need a cluster** (`tests/cluster.rs`): one test, gated on **`ADAM_TEST_KUBECONFIG`**, that makes a pod from the
  chart's own template, runs commands in it through `adam-kube-exec` (both streams, the exit code, the working directory,
  variables, every word its own argument, stdin and `/dev/null`, 3 MB of output, what a spec hides, that the coder's secrets are
  not in the pod), kills a command (the client alone leaves it, `kill` stops it and ends the client), sweeps an idle pod and not
  a busy one, releases, and checks that the chart's admission policy refuses each of thirteen changes to the pod while accepting
  the unchanged one, and (with `ADAM_TEST_KUBE_QUOTA_PODS`) that the quota refuses one pod beyond its limit with
  `EnvError::Unavailable`. The kubeconfig must be **the coder's ServiceAccount's**, or the policy tests nothing. Without the
  variable the test says it skipped; **with `ADAM_TEST_REQUIRE_KUBERNETES=1` it fails instead** (the `run-pods` job of CI sets
  it). The other variables (`ADAM_TEST_KUBE_TEMPLATE`, `ADAM_TEST_KUBE_NAMESPACE`, `ADAM_TEST_KUBE_INSTANCE`,
  `ADAM_TEST_KUBE_WORKER`, `ADAM_TEST_KUBE_MODEL_KEY`) are in the header of the file, and the script that sets a `kind` cluster up
  is [`deploy/coder/tests/kind-run-pods.sh`](../../deploy/coder/tests/kind-run-pods.sh). The `NetworkPolicy` is not exercised: it
  needs a CNI that enforces it.
* `ADAM_TEST_REQUIRE_DB=1`, which CI sets for the database suites, does **not** turn this test on: a runner without a cluster
  would fail every other job.
