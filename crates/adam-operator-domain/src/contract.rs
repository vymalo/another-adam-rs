//! The env contract of `adam-coder` and `adam-agent`: every variable name, path and default the
//! operator knows about adam, and the only place that does.
//!
//! Read from this repository: `docs/reference/environment.md`, `bin/adam-coder/README.md`,
//! `bin/adam-agent/README.md`, `docker/coder/Dockerfile` and `deploy/coder/templates`. The parity
//! goldens (`tests/golden/`) hold it equal to what the chart renders, and `tools/adam-operator-parity/regen.sh
//! --check` fails when the chart has moved.

/// The members of a chat request the runtime owns: `MODEL_EXTRA_BODY` may not set them (the process
/// refuses it with exit 78, as the chart does at render time).
pub const MODEL_EXTRA_BODY_OWNED: [&str; 5] =
    ["model", "messages", "tools", "tool_choice", "stream"];

/// Variable names.
pub mod env {
    /// Where the process listens.
    pub const LISTEN_ADDR: &str = "LISTEN_ADDR";
    /// `all` (unset), `control-plane` or `worker`.
    pub const ROLE: &str = "ROLE";
    /// Where clients reach the JSON-RPC endpoint (the card).
    pub const PUBLIC_URL: &str = "PUBLIC_URL";
    /// Comma-separated bearer tokens of the A2A server (a Secret).
    pub const A2A_BEARER_TOKENS: &str = "A2A_BEARER_TOKENS";
    /// The Postgres run store (a Secret).
    pub const DATABASE_URL: &str = "DATABASE_URL";
    /// The OpenAI-compatible gateway, with its `/v1`.
    pub const MODEL_BASE_URL: &str = "MODEL_BASE_URL";
    /// Its key (a Secret).
    pub const MODEL_API_KEY: &str = "MODEL_API_KEY";
    /// The model alias.
    pub const MODEL: &str = "MODEL";
    /// A JSON object merged into every chat request (not a secret).
    pub const MODEL_EXTRA_BODY: &str = "MODEL_EXTRA_BODY";
    /// `reasoning_content` or `reasoning`: send earlier reasoning back under that name.
    pub const MODEL_ECHO_REASONING: &str = "MODEL_ECHO_REASONING";
    /// The model's context window, in tokens: what each usage report says as its window.
    pub const MODEL_CONTEXT_WINDOW: &str = "MODEL_CONTEXT_WINDOW";
    /// The alias OpenCode uses.
    pub const OPENCODE_MODEL: &str = "OPENCODE_MODEL";
    /// Runs advanced at once, per process.
    pub const WORKERS: &str = "WORKERS";
    /// Failed checks before the coder stops.
    pub const MAX_CHECK_CYCLES: &str = "MAX_CHECK_CYCLES";
    /// Time limit of one check.
    pub const CHECK_TIMEOUT_SECS: &str = "CHECK_TIMEOUT_SECS";
    /// How often finished workspaces are swept.
    pub const WORKSPACE_SWEEP_SECS: &str = "WORKSPACE_SWEEP_SECS";
    /// Mirrors and worktrees.
    pub const WORKSPACE_ROOT: &str = "WORKSPACE_ROOT";
    /// `shared`, `affinity` or `isolated`.
    pub const WORKSPACE_PLACEMENT: &str = "WORKSPACE_PLACEMENT";
    /// The lease identity of a worker; the pod name when placement pins runs.
    pub const WORKER_ID: &str = "WORKER_ID";
    /// Let a folder's `mcp.json` start local processes.
    pub const MCP_ALLOW_STDIO: &str = "MCP_ALLOW_STDIO";
    /// Let it reach plain-http MCP servers on other machines.
    pub const MCP_ALLOW_INSECURE: &str = "MCP_ALLOW_INSECURE";
    /// The file of extra MCP servers.
    pub const ADAM_EXTRA_MCP_FILE: &str = "ADAM_EXTRA_MCP_FILE";
    /// The agent folder (`adam-agent`).
    pub const ADAM_AGENT_DIR: &str = "ADAM_AGENT_DIR";
    /// Where the GitHub MCP sidecar is.
    pub const GITHUB_MCP_URL: &str = "GITHUB_MCP_URL";
    /// Hosts repositories may live on, comma-joined.
    pub const ALLOWED_REPO_HOSTS: &str = "ALLOWED_REPO_HOSTS";
    /// The GitHub REST API root.
    pub const GITHUB_API_URL: &str = "GITHUB_API_URL";
    /// Owners `create_repository` may create for, comma-joined.
    pub const CREATE_REPO_OWNERS: &str = "CREATE_REPO_OWNERS";
    /// GitHub App mode: the App's id.
    pub const GITHUB_APP_ID: &str = "GITHUB_APP_ID";
    /// App mode: one pinned installation.
    pub const GITHUB_APP_INSTALLATION_ID: &str = "GITHUB_APP_INSTALLATION_ID";
    /// App mode: the accounts the App may act for, comma-joined.
    pub const GITHUB_APP_OWNERS: &str = "GITHUB_APP_OWNERS";
    /// App mode: the key's file.
    pub const GITHUB_APP_PRIVATE_KEY_PATH: &str = "GITHUB_APP_PRIVATE_KEY_PATH";
    /// Token mode (a Secret).
    pub const GITHUB_TOKEN: &str = "GITHUB_TOKEN";
    /// Open pull requests as drafts.
    pub const PR_DRAFT: &str = "PR_DRAFT";
    /// The commits' author name.
    pub const GIT_AUTHOR_NAME: &str = "GIT_AUTHOR_NAME";
    /// The commits' author email.
    pub const GIT_AUTHOR_EMAIL: &str = "GIT_AUTHOR_EMAIL";
    /// `GITHUB_HOST` of the sidecar (GitHub Enterprise).
    pub const GITHUB_HOST: &str = "GITHUB_HOST";

    /// Every variable the operator can set on an agent container. `extraEnv` may name none of them,
    /// whether or not the operator sets it for this agent.
    pub const OPERATOR_SET: &[&str] = &[
        LISTEN_ADDR,
        ROLE,
        PUBLIC_URL,
        A2A_BEARER_TOKENS,
        DATABASE_URL,
        MODEL_BASE_URL,
        MODEL_API_KEY,
        MODEL,
        MODEL_EXTRA_BODY,
        MODEL_ECHO_REASONING,
        MODEL_CONTEXT_WINDOW,
        OPENCODE_MODEL,
        WORKERS,
        MAX_CHECK_CYCLES,
        CHECK_TIMEOUT_SECS,
        WORKSPACE_SWEEP_SECS,
        WORKSPACE_ROOT,
        WORKSPACE_PLACEMENT,
        WORKER_ID,
        MCP_ALLOW_STDIO,
        MCP_ALLOW_INSECURE,
        ADAM_EXTRA_MCP_FILE,
        ADAM_AGENT_DIR,
        GITHUB_MCP_URL,
        ALLOWED_REPO_HOSTS,
        GITHUB_API_URL,
        CREATE_REPO_OWNERS,
        GITHUB_APP_ID,
        GITHUB_APP_INSTALLATION_ID,
        GITHUB_APP_OWNERS,
        GITHUB_APP_PRIVATE_KEY_PATH,
        GITHUB_TOKEN,
        PR_DRAFT,
        GIT_AUTHOR_NAME,
        GIT_AUTHOR_EMAIL,
    ];
}

/// The port the A2A server and a worker's `/healthz` listen on.
pub const PORT: u16 = 8080;

/// `LISTEN_ADDR`.
pub const LISTEN_ADDR: &str = "0.0.0.0:8080";

/// The health route (`adam-service`), for the startup, liveness and readiness probes.
pub const HEALTHZ: &str = "/healthz";

/// The agent container's name in the pod.
pub const AGENT_CONTAINER: &str = "agent";

/// The command of `adam-agent`: the image's entrypoint is `tini -- adam-coder`, so a folder agent
/// overrides it (`docker/coder/Dockerfile`; `bin/adam-agent/README.md`, "Image and compose").
pub const ADAM_AGENT_COMMAND: [&str; 3] = ["tini", "--", "adam-agent"];

/// Where a folder is mounted, and `ADAM_AGENT_DIR`: the directory that is `agent/` itself.
pub const AGENT_DIR: &str = "/etc/adam/agent";

/// The volume of the folder.
pub const AGENT_FOLDER_VOLUME: &str = "agent-folder";

/// Where the extra MCP file is mounted.
pub const EXTRA_MCP_DIR: &str = "/etc/adam/extra-mcp";

/// `ADAM_EXTRA_MCP_FILE`.
pub const EXTRA_MCP_FILE: &str = "/etc/adam/extra-mcp/mcp.json";

/// The file name inside the mount.
pub const EXTRA_MCP_FILE_NAME: &str = "mcp.json";

/// The volume of the extra MCP file.
pub const EXTRA_MCP_VOLUME: &str = "extra-mcp";

/// Where the GitHub App key is mounted, as one file.
pub const GITHUB_APP_DIR: &str = "/var/run/secrets/github-app";

/// The key's file name, and `GITHUB_APP_PRIVATE_KEY_PATH` is `GITHUB_APP_DIR/GITHUB_APP_KEY_FILE`.
pub const GITHUB_APP_KEY_FILE: &str = "private-key.pem";

/// The volume of the key.
pub const GITHUB_APP_VOLUME: &str = "github-app";

/// The volume the coder's workspace is. `WORKSPACE_ROOT` is where it is mounted.
pub const WORK_VOLUME: &str = "work";

/// Volume names the operator uses itself.
pub const RESERVED_VOLUMES: &[&str] = &[AGENT_FOLDER_VOLUME, EXTRA_MCP_VOLUME, GITHUB_APP_VOLUME];

/// Mount paths the operator uses itself.
pub const RESERVED_MOUNTS: &[&str] = &[AGENT_DIR, EXTRA_MCP_DIR, GITHUB_APP_DIR];

/// The permission bits of mounted ConfigMap files: readable by the runtime user whatever `fsGroup`.
pub const FILES_MODE: u32 = 0o444;

/// The permission bits of the key: group-readable by the runtime user (`fsGroup`), for the coder alone.
pub const KEY_MODE: u32 = 0o440;

/// The user, group and `fsGroup` of the adam image (the `agent` user of the workspace image).
pub const DEFAULT_ID: i64 = 10001;

/// The folder's size limit in v0: a ConfigMap holds 1 MiB.
pub const FOLDER_LIMIT_BYTES: usize = 1024 * 1024;

/// Probe periods of the chart (`deploy/coder/templates/statefulset.yaml`).
pub mod probes {
    /// Startup: seconds between probes.
    pub const STARTUP_PERIOD: u32 = 3;
    /// Startup: failures allowed.
    pub const STARTUP_FAILURES: u32 = 40;
    /// Liveness: seconds between probes.
    pub const LIVENESS_PERIOD: u32 = 15;
    /// Liveness: seconds a probe may take.
    pub const LIVENESS_TIMEOUT: u32 = 3;
    /// Liveness: failures allowed.
    pub const LIVENESS_FAILURES: u32 = 4;
    /// Readiness: seconds between probes.
    pub const READINESS_PERIOD: u32 = 5;
    /// Readiness: seconds a probe may take.
    pub const READINESS_TIMEOUT: u32 = 3;
}

/// The GitHub MCP server sidecar (`ADR 0017, D4` of adam-rs; the chart's `githubMcp`).
pub mod sidecar {
    /// The container's name.
    pub const NAME: &str = "github-mcp";
    /// Startup probe period of the sidecar.
    pub const STARTUP_PERIOD: u32 = 2;
    /// Startup probe failures allowed.
    pub const STARTUP_FAILURES: u32 = 30;
    /// Default port.
    pub const DEFAULT_PORT: u16 = 8082;
    /// Its own small defaults (the chart's `githubMcp.resources`).
    pub const REQUEST_CPU: &str = "50m";
    /// Memory request.
    pub const REQUEST_MEMORY: &str = "64Mi";
    /// Memory limit.
    pub const LIMIT_MEMORY: &str = "256Mi";

    /// The command: `tini -- github-mcp-server http …`, read-only, four toolsets, on loopback, with
    /// no credential: the coder sends the credentials of each call.
    pub fn command_and_args(port: u16) -> (Vec<String>, Vec<String>) {
        let command = ["tini", "--", "github-mcp-server"]
            .map(str::to_owned)
            .to_vec();
        let args = [
            "http",
            "--read-only",
            "--toolsets",
            "context,repos,issues,pull_requests",
            "--listen-host",
            "127.0.0.1",
            "--port",
        ]
        .map(str::to_owned)
        .into_iter()
        .chain([port.to_string()])
        .collect();
        (command, args)
    }

    /// An `exec` probe, not a TCP one: the kubelet's TCP probe connects to the pod IP and the server
    /// listens on loopback only (adam-rs #86).
    pub fn startup_probe_command(port: u16) -> Vec<String> {
        vec![
            "bash".to_owned(),
            "-c".to_owned(),
            format!("exec 3<>/dev/tcp/127.0.0.1/{port}"),
        ]
    }
}

/// The front of `split` (`deploy/coder/templates/front-deployment.yaml`).
pub mod front {
    /// Seconds a front pod has to stop.
    pub const TERMINATION_GRACE_SECS: u32 = 30;
    /// Requested CPU.
    pub const REQUEST_CPU: &str = "100m";
    /// Requested memory.
    pub const REQUEST_MEMORY: &str = "256Mi";
    /// Memory limit.
    pub const LIMIT_MEMORY: &str = "512Mi";
    /// Pods that must stay up when the front has more than one replica.
    pub const MIN_AVAILABLE: u32 = 1;
}
