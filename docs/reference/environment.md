# Environment variables

Configuration of both agents is environment variables only. Problems are reported all at once at startup
(`invalid configuration`, exit 78). Sources of truth: `crates/adam-service/src/config.rs` (the common ones),
`bin/adam-coder/src/config.rs`, `bin/adam-agent/src/config.rs`, `crates/adam-env-kubernetes/src/settings.rs`.
The chart sets these from its values: [chart README](../../deploy/coder/README.md). Secrets are never in
files or logs; they are `SecretString`s and absent from `Debug`.

The roles that run workers (`all`, `worker`) read everything below except where a row says otherwise.
A `control-plane` reads only `ROLE`, `DATABASE_URL`, `A2A_BEARER_TOKENS`, `PUBLIC_URL`, `LISTEN_ADDR`,
`ADAM_AGENT_DIR` and the optional `A2A_PUSH_*` and `A2A_CARD_SIGNING_*`, and a chart may set the rest for every role.

## Both binaries (`adam-service`)

| Variable | Meaning | Default |
|---|---|---|
| `ROLE` | `all`, `control-plane` or `worker` | `all` |
| `DATABASE_URL` | Postgres for the run store | required |
| `A2A_BEARER_TOKENS` | comma-separated accepted tokens; **none = no server** | required by `all`, `control-plane` |
| `PUBLIC_URL` | where clients reach the JSON-RPC endpoint (goes in the agent card) | required by `all`, `control-plane` |
| `LISTEN_ADDR` | the A2A server, or a worker's `/healthz` | `0.0.0.0:8080` |
| `WORKERS` | runs advanced concurrently | `4` |
| `WORKER_ID` | lease identity: 1 to 128 of letters, digits, `.`, `_`, `-`, not starting with `.` | random per process; required by `affinity`, `isolated` |
| `MODEL_BASE_URL`, `MODEL_API_KEY` | OpenAI-compatible gateway (with `/v1`) and key | required by `all`, `worker` (key may be empty) |
| `MODEL` | the agent's model alias | required by `all`, `worker` |
| `MODEL_EXTRA_BODY` | JSON object merged into every chat request, e.g. `{"reasoning_effort":"medium"}`; **not a secret**; may not set `model`, `messages`, `tools`, `tool_choice`, `stream` | unset |
| `MODEL_ECHO_REASONING` | `reasoning_content` or `reasoning`: send earlier reasoning back under that name (DeepSeek thinking mode with tools needs it) | unset |
| `MCP_ALLOW_STDIO` | let `mcp.json` start local processes | `false` |
| `MCP_ALLOW_INSECURE` | allow plain `http` MCP servers on other machines; development only | `false` |
| `MCP_ALLOW_URL_VARS` | allow `${VAR}` in a server `url` (the SDK logs URLs) | `false` |
| `THREAD_TOOLS_MAX_CALL_SECS` | longest wait for a thread-tool call (1 to 86400) | `3600` |
| `RUST_LOG` | log filter; JSON logs on stdout; replaces the default whole | `info,rmcp=warn` |

### Optional A2A features (roles that serve A2A: `all`, `control-plane`)

Nothing here is on by default; a role that does not serve A2A reads none of it. A bad value is a startup problem (exit 78).
[ADR 0030](../decisions/0030-a2a-push-notifications-list-tasks-extended-card-signatures.md), [what a client sees](a2a-server.md).

| Variable | Meaning | Default |
|---|---|---|
| `A2A_PUSH_ALLOWED_URLS` | **turns push notifications on** and names the webhooks they may reach: comma-separated URL prefixes (`https://hooks.example.com/a2a/`) or hosts (`hooks.example.com`, `*.example.com`, `host:8443`). A client's webhook that matches none, is not `https` or is a private address is refused | unset: off, the card says `pushNotifications: false` |
| `A2A_PUSH_ALLOW_PRIVATE` | also allow webhooks on loopback, private and link-local addresses, and `http` to loopback. **Development only** | `false` |
| `A2A_PUSH_GIVE_UP_AFTER_SECS` | how long one notification may keep failing before delivery to that webhook is abandoned (1 to 604800) | `3600` |
| `A2A_PUSH_REQUEST_TIMEOUT_SECS` | how long one request to a webhook may take (1 to 120) | `15` |
| `A2A_CARD_SIGNING_KEY_FILE` | a PKCS#8 PEM private key, ECDSA P-256 or Ed25519, that signs the public and the extended card; mount it from a Secret; must exist and be usable | unset: the card is unsigned |
| `A2A_CARD_SIGNING_KEY_ID` | the signature's `kid`; needs the key file | the key's RFC 7638 thumbprint |
| `A2A_CARD_SIGNING_JKU` | the `jku` in the signature's header (where clients fetch the key set; the server serves it at `/.well-known/jwks.json`); needs the key file | unset |

## Agent folder (both binaries)

| Variable | Meaning | Default |
|---|---|---|
| `ADAM_AGENT_DIR` | the folder of `agent/` files, read once at startup by every role; must exist (else exit 78). Required by `adam-agent` | `adam-coder`: its embedded copy |
| `ADAM_EXTRA_MCP_FILE` | a file of extra MCP servers in the shape of `mcp.json`, **added** to the agent's own (a name clash is exit 78) | unset |

## The coder (`adam-coder`)

| Variable | Meaning | Default |
|---|---|---|
| `OPENCODE_MODEL` | model alias OpenCode uses through the same gateway | `MODEL` |
| `OPENCODE_COMMAND` | the ACP program and arguments | `opencode acp` |
| `GITHUB_TOKEN` | push and pull request token, sent only to `ALLOWED_REPO_HOSTS`. Unset in App mode | one of this or the App's |
| `GITHUB_APP_ID` | App mode: the App's id (the JWT's `iss`) | required in App mode |
| `GITHUB_APP_INSTALLATION_ID` | **pins** the App to one installation | this or `GITHUB_APP_OWNERS`, exactly one |
| `GITHUB_APP_OWNERS` | no pin: the accounts the App may act for (comma or space list, or `*`); the installation of each owner is found with the JWT | as above |
| `GITHUB_APP_PRIVATE_KEY_PATH`, `GITHUB_APP_PRIVATE_KEY` | the App's PEM key, a file or inline; exactly one; parsed at startup | one required in App mode |
| `ALLOWED_REPO_HOSTS` | hosts repositories may live on (`name` or `name:port`); the token is scoped to them; the first is what `owner/name` means | `github.com` |
| `GITHUB_API_URL` | GitHub REST API root (Enterprise: `https://<host>/api/v3`) | `https://api.github.com` |
| `GITHUB_MCP_URL` | origin of the GitHub MCP server (the sidecar); no credentials | `http://127.0.0.1:8082` |
| `CREATE_REPO_OWNERS` | owners `create_repository` may create for, after the person agrees; empty turns the tool off | empty |
| `ALLOW_LOCAL_REPOS` | accept local paths, `file://`, plain `http://`; development and tests only | `false` |
| `WORKSPACE_ROOT` | mirrors, run workspaces, run notes | `/work` |
| `WORKSPACE_PLACEMENT` | `shared`, `affinity` or `isolated` ([placement](workspace-and-environments.md#placement-which-worker-holds-a-runs-files)) | `shared` |
| `WORKSPACE_SWEEP_SECS` | how often the janitor removes finished runs' workspaces; `0` is off | `300` |
| `MAX_CHECK_CYCLES` | failed `run_checks` in a repository before the agent must stop | `3` |
| `SCRATCH_CHECK_CYCLES` | the same for a scratch project, counted apart | `5` |
| `CHECK_TIMEOUT_SECS`, `CHECK_OUTPUT_TAIL_BYTES` | limits of one `run_checks`, `run_command` or `run` | `900`, `16384` |
| `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL` | identity of the commits | `adam-coder`, `adam-coder@users.noreply.github.com` |
| `PR_DRAFT` | open pull requests as drafts | `false` |

## Where a run's processes run (`adam-coder`)

| Variable | Meaning | Default |
|---|---|---|
| `RUN_ENVIRONMENT` | `local` or `kubernetes` (a pod for each run; refused with `DEVCONTAINER_RUNTIME=podman`) | `local` |
| `RUN_POD_TEMPLATE_FILE` | with `kubernetes`: the Pod every run pod is made from (the chart's ConfigMap) | required |
| `RUN_POD_NAMESPACE` | the namespace of run pods | the ServiceAccount's namespace |
| `RUN_POD_INSTANCE` | the release's `app.kubernetes.io/instance`, which finds this deployment's pods | required |
| `RUN_POD_CONTAINER` | the template container commands run in | `run` |
| `RUN_POD_EXEC_BINARY` | the program that runs a command in a pod | `adam-kube-exec` |
| `RUN_POD_READY_TIMEOUT_SECS` | how long a pod may take to be ready | `300` (chart: `600`) |
| `RUN_POD_IDLE_SECS` | idle time before a pod is deleted; `0` keeps it until the run ends | `900` |
| `RUN_POD_WAIT_SECS` | how long a run retries for a pod the cluster cannot give now | `600` |
| `DEVCONTAINER_RUNTIME` | `off` or `podman` (the repository's devcontainer on a rootless Podman service) | `off` |
| `CONTAINER_HOST` | Podman's own variable, e.g. `unix:///run/podman/podman.sock`; required with `podman` | unset |
| `DEVCONTAINER_DEFAULT_IMAGE` | image for a repository with no `devcontainer.json`; **by digest only** | the `workspace` image the coder is built on |
| `DEVCONTAINER_NETWORK` | `inherit` or `none` (then `delegate_to_opencode` refuses) | `inherit` |
| `DEVCONTAINER_DEPLOYMENT_ID` | label the orphan sweep finds this deployment's containers by | `WORKER_ID`, else `adam-coder` |
| `DEVCONTAINER_UP_TIMEOUT_SECS`, `DEVCONTAINER_SETUP_TIMEOUT_SECS` | pulling, building and creating; lifecycle commands | `1200`, `900` |
| `DEVCONTAINER_PREPULL` | pull the default image at startup | `true` |
| `DEVCONTAINER_CLI`, `DEVCONTAINER_PODMAN` | the devcontainer CLI and Podman's remote client | `devcontainer`, `podman-remote` |
| `OPENCODE_BINARY` | with `podman`: the native OpenCode executable mounted into containers | `OPENCODE_COMMAND`'s program |

## Local stack (`compose.yaml`)

Put these in a `.env` next to `compose.yaml`.

| Variable | Meaning | Default |
|---|---|---|
| `POSTGRES_PORT`, `MONGODB_PORT` | host ports of the databases | `5432`, `27017` |
| `MOCK_OPENAI_PORT`, `MOCK_GITHUB_PORT`, `MOCK_GITHUB_MCP_PORT`, `GIT_SERVER_PORT` | host ports of the mocks | `8081`, `8082`, `8085`, `8083` |
| `CODER_PORT`, `AGENT_PORT` | host ports of the coder and the general agent | `8080`, `8084` |
| `ADAM_BUILD_REVISION` | build argument, not a runtime variable: the commit baked into both binaries, shown on the card as the version's `+<sha7>` and in `build/v1` ([ADR 0028](../decisions/0028-the-card-says-which-build-answers.md)). Unset says `unknown` | unset |
| `CODER_IMAGE` | image of both agents; with `--no-build` runs a prebuilt one | `adam-rs/coder:dev` |
| `CODER_MODEL`, `CODER_OPENCODE_MODEL`, `AGENT_MODEL` | the scripted models | `mock-coder`, `mock-opencode`, `mock-assistant` |
| `CODER_AGENT_DIR`, `AGENT_FOLDER` | the agent folders mounted at `/etc/adam/agent` | `./bin/adam-coder/agent`, `./dev/agents/assistant/agent` |

## Tests

| Variable | Meaning |
|---|---|
| `ADAM_TEST_POSTGRES_URL`, `ADAM_TEST_MONGODB_URI` | the database suites; skipped when unset |
| `ADAM_TEST_REQUIRE_DB=1` | a suite whose variable is unset **fails** instead of skipping (CI sets it) |
| `ADAM_TEST_MOCK_OPENAI_URL`, `ADAM_TEST_MOCK_GITHUB_URL`, `ADAM_TEST_MOCK_GITHUB_MCP_URL` | the tests of the real clients against the compose mocks |
| `ADAM_TEST_DEVCONTAINER=1`, `ADAM_TEST_REQUIRE_DEVCONTAINER=1` | the devcontainer suite; the second fails instead of skipping |
| `ADAM_TEST_KUBECONFIG`, `ADAM_TEST_REQUIRE_KUBERNETES=1` | the cluster test of `adam-env-kubernetes` (more `ADAM_TEST_KUBE_*` in `crates/adam-env-kubernetes/tests/cluster.rs`) |
| `ADAM_TEST_OPENAI_BASE_URL`, `ADAM_TEST_OPENAI_API_KEY`, `ADAM_TEST_OPENAI_MODEL` | a live model for `adam-model-openai/tests/live.rs` |
| `ADAM_TEST_OPENCODE=1` | the live OpenCode test of `adam-acp` |
| `ADAM_TRYBUILD=1` | the rustc-worded half of the `#[tool]` compile tests |
| `ADAM_UPDATE_GOLDEN=1` | regenerate the codegen golden of `adam-agent-fs` |

See [Testing](../guides/testing.md).
