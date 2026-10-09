//! `validate`: the rules the CRD schema leaves to the reconciler, because they need a second object or more
//! than a CRD schema can say. Each is reported with the field it is about; none stops at the first.

use std::collections::BTreeSet;
use std::fmt;

use adam_operator_api::{
    AgentConfig, AgentService, Binary, MAX_CONTEXT_WINDOW, SecretKeyRef, Topology, VolumeScope,
};

use crate::contract::{self, env};
use crate::syntax::{
    is_dns_label, is_env_name, is_http_token, is_mount_path, is_plain_http_remote,
    is_relative_file_path, is_size, parse_http_url, paths_overlap,
};
use crate::{convert, plan};

/// One reason an `AgentService` and its `AgentConfig` cannot be resolved. The controller reports
/// them as `ConfigResolved: False` with the reason `ConfigInvalid`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigIssue {
    /// The field, as a path from the object: `AgentConfig spec.tools.mcpServers[search].url`.
    pub field: String,
    /// What is wrong, and what to do about it.
    pub message: String,
}

impl fmt::Display for ConfigIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

/// The longest service name: the StatefulSet's pod label `controller-revision-hash` is the name plus
/// eleven characters and a label value is at most 63 (*unverified* here: Kubernetes issue 64023).
const MAX_NAME: usize = 52;

struct Issues(Vec<ConfigIssue>);

impl Issues {
    fn add(&mut self, field: impl Into<String>, message: impl Into<String>) {
        self.0.push(ConfigIssue {
            field: field.into(),
            message: message.into(),
        });
    }
}

/// Check the rules the reconciler owns: the cross-object ones and the ones of the adam-rs chart's
/// `_validate.tpl`, so the same mistakes fail the same way.
///
/// It also re-checks the shape rules the CRD's CEL states (one of two, a block that goes with a
/// binary): `resolve` relies on them, and an object that reached the controller without an API
/// server's CEL (a test, a CRD installed without the rules) must not be resolved into something
/// half-made.
///
/// # Errors
///
/// Every issue found, in a fixed order: the service, then the config.
pub fn validate(service: &AgentService, config: &AgentConfig) -> Result<(), Vec<ConfigIssue>> {
    let mut v = Issues(Vec::new());
    check_service(&mut v, service, config);
    check_config(&mut v, config);
    if v.0.is_empty() { Ok(()) } else { Err(v.0) }
}

/// Check an `AgentConfig` by itself: the rules of [`validate`] that need no `AgentService`. The
/// `AgentConfig` controller reports them as its `Valid` condition. A config
/// that passes can still fail [`validate`] against a service: the rules that read both (a placement
/// that fits the number of workers, a reference that names this config) are the service's.
///
/// # Errors
///
/// Every issue found, in the order [`validate`] reports the config's.
pub fn validate_config(config: &AgentConfig) -> Result<(), Vec<ConfigIssue>> {
    let mut v = Issues(Vec::new());
    check_config(&mut v, config);
    if v.0.is_empty() { Ok(()) } else { Err(v.0) }
}

fn secret_ref(v: &mut Issues, field: &str, r: &SecretKeyRef) {
    if r.name.trim().is_empty() || r.key.trim().is_empty() {
        v.add(field, "a Secret reference needs a name and a key");
    }
}

fn check_service(v: &mut Issues, svc: &AgentService, cfg: &AgentConfig) {
    let me = "AgentService";
    let name = svc.metadata.name.as_deref().unwrap_or_default();
    if !is_dns_label(name) || name.len() > MAX_NAME {
        v.add(
            format!("{me} metadata.name"),
            format!("{name:?} is not a DNS label of at most {MAX_NAME} characters: the operator derives `<name>-front`, `<name>-agent-<hash>` and `<name>-db-app` from it"),
        );
    }
    let ns = svc.metadata.namespace.as_deref().unwrap_or_default();
    if ns.is_empty() {
        v.add(
            format!("{me} metadata.namespace"),
            "the object has no namespace",
        );
    }
    if svc.spec.config_ref.name != cfg.metadata.name.clone().unwrap_or_default() {
        v.add(
            format!("{me} spec.configRef.name"),
            format!(
                "names {:?}, and the AgentConfig given is {:?}",
                svc.spec.config_ref.name,
                cfg.metadata.name.clone().unwrap_or_default()
            ),
        );
    }
    if let (Some(a), Some(b)) = (&svc.metadata.namespace, &cfg.metadata.namespace)
        && a != b
    {
        v.add(
            format!("{me} spec.configRef"),
            format!("the AgentConfig is in namespace {b:?}, the service in {a:?}: a reference is to the same namespace"),
        );
    }

    let a2a = &svc.spec.interfaces.a2a;
    if !a2a.enabled {
        v.add(
            format!("{me} spec.interfaces.a2a.enabled"),
            "false: the adam binaries serve A2A and fail closed without a bearer token (exit 78), so a service with A2A off would not start; v0 has no other surface",
        );
    }
    match &a2a.bearer_tokens_secret_ref {
        Some(r) => secret_ref(v, &format!("{me} spec.interfaces.a2a.bearerTokensSecretRef"), r),
        None => v.add(
            format!("{me} spec.interfaces.a2a.bearerTokensSecretRef"),
            "no token, no server: the agent fails closed, so the Secret key that holds A2A_BEARER_TOKENS is required",
        ),
    }
    if let Some(url) = a2a.public_url.as_deref().filter(|u| !u.is_empty())
        && let Err(why) = parse_http_url(url, false)
    {
        v.add(
            format!("{me} spec.interfaces.a2a.publicUrl"),
            format!("{url:?}: {why}"),
        );
    }
    if svc.spec.interfaces.responses.enabled || svc.spec.interfaces.mcp.enabled {
        v.add(
            format!("{me} spec.interfaces"),
            "responses and mcp cannot be enabled: v0 serves A2A only",
        );
    }

    let scaling = &svc.spec.scaling;
    if scaling.workers < 1 {
        v.add(format!("{me} spec.scaling.workers"), "at least one worker");
    }
    if scaling.front.is_some() && scaling.topology != Topology::Split {
        v.add(
            format!("{me} spec.scaling.front"),
            "only allowed with topology: split",
        );
    }
    if scaling.front.as_ref().is_some_and(|f| f.replicas < 1) {
        v.add(
            format!("{me} spec.scaling.front.replicas"),
            "at least one replica",
        );
    }
    // The chart's rule: more than one worker needs a placement, or a run that lands on a worker
    // without its worktree forks into a second pull request. Only the coder has a worktree.
    if plan::is_coder(cfg) && scaling.workers > 1 && plan::placement(cfg).is_empty() {
        v.add(
            format!("{me} spec.scaling.workers"),
            format!(
                "{} workers need AgentConfig spec.harness.adam.coder.workspacePlacement (shared, affinity or isolated): runs move between workers at every step, and a run that lands on a worker without its worktree would fork into a second pull request",
                scaling.workers
            ),
        );
    }

    let store = &svc.spec.store.postgres;
    match (&store.secret_ref, &store.cnpg) {
        (Some(r), None) => secret_ref(v, &format!("{me} spec.store.postgres.secretRef"), r),
        (None, Some(c)) => {
            if c.instances < 1 {
                v.add(
                    format!("{me} spec.store.postgres.cnpg.instances"),
                    "at least one instance",
                );
            }
            if !is_size(&c.storage.size.0) {
                v.add(
                    format!("{me} spec.store.postgres.cnpg.storage.size"),
                    format!("{:?} is not a size such as 5Gi", c.storage.size.0),
                );
            }
        }
        _ => v.add(
            format!("{me} spec.store.postgres"),
            "exactly one of secretRef and cnpg",
        ),
    }

    let (_, problems) = convert::peers(&svc.spec.access.allow_from);
    for (field, message) in problems {
        v.add(format!("{me} {field}"), message);
    }
}

fn check_config(v: &mut Issues, cfg: &AgentConfig) {
    let me = "AgentConfig";
    let spec = &cfg.spec;
    let adam = &spec.harness.adam;
    let coder = adam.coder.as_ref();

    // Shape: what the CEL rules say, which `resolve` relies on.
    match adam.binary {
        Binary::AdamCoder => {
            if adam.agent.embedded.is_none() || adam.agent.folder.is_some() || coder.is_none() {
                v.add(
                    format!("{me} spec.harness.adam"),
                    "binary adam-coder needs agent.embedded and the coder block, and no folder",
                );
            }
        }
        Binary::AdamAgent => {
            if adam.agent.folder.is_none() || adam.agent.embedded.is_some() || coder.is_some() {
                v.add(
                    format!("{me} spec.harness.adam"),
                    "binary adam-agent needs agent.folder and no coder block",
                );
            }
        }
    }

    if let Some(folder) = &adam.agent.folder {
        check_folder(v, folder);
    }
    if let Some(coder) = coder {
        check_coder(v, cfg, coder);
    }

    // Model.
    let model = &spec.model;
    if model.model.trim().is_empty() {
        v.add(format!("{me} spec.model.model"), "the model alias is empty");
    }
    match (&model.base_url.value, &model.base_url.secret_ref) {
        (Some(url), None) => {
            if let Err(why) = parse_http_url(url, false) {
                v.add(
                    format!("{me} spec.model.baseUrl.value"),
                    format!("{url:?}: {why}"),
                );
            }
        }
        (None, Some(r)) => secret_ref(v, &format!("{me} spec.model.baseUrl.secretRef"), r),
        _ => v.add(
            format!("{me} spec.model.baseUrl"),
            "exactly one of value and secretRef",
        ),
    }
    secret_ref(
        v,
        &format!("{me} spec.model.apiKeySecretRef"),
        &model.api_key_secret_ref,
    );

    if let Some(body) = &model.extra_body {
        for key in contract::MODEL_EXTRA_BODY_OWNED {
            if body.contains_key(key) {
                v.add(
                    format!("{me} spec.model.extraBody.{key}"),
                    "the runtime owns this member of every request: extraBody may not set it",
                );
            }
        }
    }
    if let Some(window) = model.context_window
        && !(1..=MAX_CONTEXT_WINDOW).contains(&window)
    {
        v.add(
            format!("{me} spec.model.contextWindow"),
            format!("{window}: a number of tokens from 1 to {MAX_CONTEXT_WINDOW} (adam exits 78 on anything else), or leave it out"),
        );
    }

    check_tools(v, cfg);
    check_environment(v, cfg);

    // extraEnv: names the operator does not set.
    let header_names: BTreeSet<&str> = plan::header_vars(&spec.tools).keys().copied().collect();
    for name in spec.extra_env.keys() {
        if !is_env_name(name) {
            v.add(
                format!("{me} spec.extraEnv.{name}"),
                "not a variable name ([A-Za-z_][A-Za-z0-9_]*)",
            );
        } else if env::OPERATOR_SET.contains(&name.as_str()) || header_names.contains(name.as_str())
        {
            v.add(
                format!("{me} spec.extraEnv.{name}"),
                "the operator sets this variable itself: use the field that becomes it (extraEnv is for names the operator does not own)",
            );
        }
    }
}

fn check_folder(v: &mut Issues, folder: &adam_operator_api::Folder) {
    let me = "AgentConfig";
    match (&folder.files, &folder.config_map_ref) {
        (Some(files), None) => {
            let mut size = 0usize;
            for (path, content) in files {
                size += path.len() + content.len();
                if !is_relative_file_path(path) {
                    v.add(
                        format!("{me} spec.harness.adam.agent.folder.files[{path}]"),
                        "not a relative path inside the folder (no leading `/`, no `.` or `..`, no empty segment)",
                    );
                }
            }
            if size > contract::FOLDER_LIMIT_BYTES {
                v.add(
                    format!("{me} spec.harness.adam.agent.folder.files"),
                    format!("{size} bytes: a ConfigMap holds 1 MiB, which is the folder's size limit in v0 (use configMapRef with a ConfigMap you manage, or a smaller folder)"),
                );
            }
            let has_instructions = files.keys().any(|p| {
                p == "instructions.md"
                    || p == "agent/instructions.md"
                    || (p.starts_with("agents/") && p.ends_with("/instructions.md"))
            });
            if !has_instructions {
                v.add(
                    format!("{me} spec.harness.adam.agent.folder.files"),
                    "no instructions.md: adam-agent finds an agent by its instructions (verified 2026-10-05, docs/authoring.md at 0391809, \"Discovery rules\")",
                );
            }
        }
        (None, Some(r)) => {
            if r.name.trim().is_empty() {
                v.add(
                    format!("{me} spec.harness.adam.agent.folder.configMapRef.name"),
                    "the name is empty",
                );
            }
        }
        _ => v.add(
            format!("{me} spec.harness.adam.agent.folder"),
            "exactly one of files and configMapRef",
        ),
    }
}

fn check_coder(v: &mut Issues, cfg: &AgentConfig, coder: &adam_operator_api::Coder) {
    let me = "AgentConfig";
    let base = "spec.harness.adam.coder";

    let placement = plan::placement(cfg);
    if placement == "a2a-only" {
        v.add(
            format!("{me} {base}.workspacePlacement"),
            "a2a-only is refused: every tool of the coder needs a workspace. Use shared, affinity or isolated",
        );
    } else if !matches!(placement.as_str(), "" | "shared" | "affinity" | "isolated") {
        v.add(
            format!("{me} {base}.workspacePlacement"),
            format!("{placement:?}: one of shared, affinity or isolated (or empty for the single-worker default)"),
        );
    }
    let work = plan::work_volume(cfg);
    let persistent = work.map(|w| &w.source.persistent);
    match placement.as_str() {
        "shared" | "affinity" => match persistent {
            Some(p) if !p.per_replica => {}
            Some(_) => v.add(
                format!("{me} spec.environment.volumes[{}]", contract::WORK_VOLUME),
                format!("placement {placement} mounts one ReadWriteMany volume for every worker: the volume `work` needs perReplica: false"),
            ),
            None => v.add(
                format!("{me} spec.environment.volumes"),
                format!("placement {placement} needs a volume named `work`, one ReadWriteMany claim (perReplica: false), whose class supports ReadWriteMany"),
            ),
        },
        "isolated" => match persistent {
            Some(p) if p.per_replica => {}
            Some(_) => v.add(
                format!("{me} spec.environment.volumes[{}]", contract::WORK_VOLUME),
                "placement isolated gives every worker a volume of its own: the volume `work` needs perReplica: true",
            ),
            None => v.add(
                format!("{me} spec.environment.volumes"),
                "placement isolated needs a volume named `work` with perReplica: true",
            ),
        },
        _ => {}
    }

    for (field, ok, what) in [
        (
            "workers",
            coder.workers.is_none_or(|n| n >= 1),
            "at least 1",
        ),
        (
            "maxCheckCycles",
            coder.max_check_cycles.is_none_or(|n| n >= 0),
            "0 or more",
        ),
        (
            "checkTimeoutSecs",
            coder.check_timeout_secs.is_none_or(|n| n >= 1),
            "at least 1",
        ),
        (
            "workspaceSweepSecs",
            coder.workspace_sweep_secs.is_none_or(|n| n >= 1),
            "at least 1",
        ),
    ] {
        if !ok {
            v.add(format!("{me} {base}.{field}"), what);
        }
    }

    if let Some(url) = coder.github_api_url.as_deref()
        && let Err(why) = parse_http_url(url, false)
    {
        v.add(
            format!("{me} {base}.githubApiUrl"),
            format!("{url:?}: {why}"),
        );
    }
    if let Some(a) = &coder.git_author
        && (a.name.trim().is_empty() || a.email.trim().is_empty())
    {
        v.add(
            format!("{me} {base}.gitAuthor"),
            "name and email are both needed",
        );
    }
    for (field, list) in [
        ("allowedRepoHosts", &coder.allowed_repo_hosts),
        ("createRepoOwners", &coder.create_repo_owners),
    ] {
        for (i, item) in list.iter().enumerate() {
            if item.trim().is_empty() || item.contains(|c: char| c == ',' || c.is_whitespace()) {
                v.add(
                    format!("{me} {base}.{field}[{i}]"),
                    format!("{item:?}: a name with no comma or space (the variable is a list joined with commas)"),
                );
            }
        }
    }

    let github = &coder.github;
    match (&github.app, &github.token) {
        (Some(app), None) => {
            if app.id.trim().is_empty() {
                v.add(
                    format!("{me} {base}.github.app.id"),
                    "the App's id is empty",
                );
            }
            match (&app.owners, app.installation_id) {
                (Some(owners), None) => {
                    let names: Vec<&str> = owners.iter().map(|o| o.trim()).collect();
                    if names.is_empty()
                        || names.iter().any(|o| o.is_empty() || o.contains([',', ' ']))
                    {
                        v.add(
                            format!("{me} {base}.github.app.owners"),
                            "names with no comma or space, and at least one",
                        );
                    }
                    if names.contains(&"*") && names.len() > 1 {
                        v.add(
                            format!("{me} {base}.github.app.owners"),
                            "`*` (every account the App is installed on) is refused beside other names",
                        );
                    }
                }
                (None, Some(id)) => {
                    if id < 1 {
                        v.add(
                            format!("{me} {base}.github.app.installationId"),
                            "a positive integer",
                        );
                    }
                }
                _ => v.add(
                    format!("{me} {base}.github.app"),
                    "exactly one of installationId and owners",
                ),
            }
            secret_ref(
                v,
                &format!("{me} {base}.github.app.privateKeySecretRef"),
                &app.private_key_secret_ref,
            );
        }
        (None, Some(token)) => secret_ref(
            v,
            &format!("{me} {base}.github.token.secretRef"),
            &token.secret_ref,
        ),
        _ => v.add(
            format!("{me} {base}.github"),
            "exactly one of app and token",
        ),
    }
}

fn check_tools(v: &mut Issues, cfg: &AgentConfig) {
    let me = "AgentConfig";
    let tools = &cfg.spec.tools;

    if let Some(g) = &tools.github_mcp {
        if !(1..=65535).contains(&g.port) {
            v.add(
                format!("{me} spec.tools.githubMcp.port"),
                format!("{}: a port number, 1 to 65535", g.port),
            );
        }
        if g.host
            .as_deref()
            .is_some_and(|h| h.contains(char::is_whitespace))
        {
            v.add(
                format!("{me} spec.tools.githubMcp.host"),
                "a host name, with no space",
            );
        }
    }

    for (id, server) in &tools.mcp_servers {
        let at = format!("{me} spec.tools.mcpServers[{id}]");
        // The model sees the tools of a server as `<id>__<tool>`, and model APIs take tool names of
        // letters, digits, `_` and `-` (*unverified* against every gateway; the operator's own rule).
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            v.add(&at, "the server's name is letters, digits, `_` and `-`: the model sees its tools as <name>__<tool>");
        }
        match parse_http_url(&server.url, true) {
            Err(why) => v.add(
                format!("{at}.url"),
                format!("{:?}: {why}. A URL is http or https with no user name, password or ${{VAR}} in it (a secret in a URL reaches the logs)", server.url),
            ),
            Ok(_) => {
                if is_plain_http_remote(&server.url) && !tools.allow_insecure_http {
                    v.add(
                        format!("{at}.url"),
                        "a plain http:// URL to another machine: adam refuses such a server at startup unless tools.allowInsecureHttp is true (knowing that it covers every MCP server of the agent and the endpoints a sender announces, and that the bearer crosses the network in the clear); or use https",
                    );
                }
            }
        }
        for (name, header) in &server.headers {
            if !is_http_token(name) {
                v.add(format!("{at}.headers[{name}]"), "not an HTTP header name");
            }
            if header.prefix.as_deref().is_some_and(|p| p.contains("${")) {
                v.add(
                    format!("{at}.headers[{name}].prefix"),
                    "plain text such as \"Bearer \": the secret is added from the Secret, never written",
                );
            }
            secret_ref(
                v,
                &format!("{at}.headers[{name}].secretRef"),
                &header.secret_ref,
            );
        }
    }

    // A header variable is named by its Secret key, so the key must be a variable name, must not be
    // one the operator sets, and must not want two different Secrets.
    for (var, wanted) in plan::header_vars(tools) {
        let at = format!("{me} spec.tools.mcpServers (header Secret key {var:?})");
        if !is_env_name(var) {
            v.add(
                &at,
                "the variable that carries a header's secret is named by the Secret key, so the key must be a variable name ([A-Za-z_][A-Za-z0-9_]*)",
            );
        } else if env::OPERATOR_SET.contains(&var) {
            v.add(&at, "this is a variable the operator sets for the agent itself: use another key in the Secret");
        }
        if wanted.len() > 1 {
            v.add(
                &at,
                format!(
                    "two headers want the variable {var} from different Secrets ({}): one variable carries one value",
                    wanted.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join(", ")
                ),
            );
        }
    }
}

fn check_environment(v: &mut Issues, cfg: &AgentConfig) {
    let me = "AgentConfig";
    let e = &cfg.spec.environment;
    let image = e.image.reference.as_str();
    if image.is_empty() || image.contains(char::is_whitespace) {
        v.add(
            format!("{me} spec.environment.image.ref"),
            "an image reference with no space",
        );
    }
    let (_, problems) = convert::resources(e.resources.as_ref());
    for (field, message) in problems {
        v.add(format!("{me} {field}"), message);
    }

    let mut names = BTreeSet::new();
    let mut mounts: Vec<&str> = Vec::new();
    for vol in &e.volumes {
        let at = format!("{me} spec.environment.volumes[{}]", vol.name);
        if !is_dns_label(&vol.name) {
            v.add(
                &at,
                "a volume's name is a DNS label: a claim is named after it",
            );
        } else if contract::RESERVED_VOLUMES.contains(&vol.name.as_str()) {
            v.add(&at, "the operator uses this volume name itself");
        }
        if !names.insert(vol.name.as_str()) {
            v.add(&at, "two volumes have this name");
        }
        if vol.scope != VolumeScope::Agent {
            v.add(
                format!("{at}.scope"),
                "v0 uses `agent`: run and project scopes are not built",
            );
        }
        if !is_mount_path(&vol.mount_path) {
            v.add(
                format!("{at}.mountPath"),
                "an absolute path with no empty, `.` or `..` segment",
            );
        } else {
            for reserved in contract::RESERVED_MOUNTS {
                if paths_overlap(&vol.mount_path, reserved) {
                    v.add(
                        format!("{at}.mountPath"),
                        format!("overlaps {reserved}, which the operator mounts itself"),
                    );
                }
            }
            for other in &mounts {
                if paths_overlap(&vol.mount_path, other) {
                    v.add(
                        format!("{at}.mountPath"),
                        format!("overlaps the mount {other} of another volume"),
                    );
                }
            }
            mounts.push(&vol.mount_path);
        }
        if !is_size(&vol.source.persistent.size.0) {
            v.add(
                format!("{at}.source.persistent.size"),
                format!(
                    "{:?} is not a size such as 20Gi",
                    vol.source.persistent.size.0
                ),
            );
        }
    }
}
