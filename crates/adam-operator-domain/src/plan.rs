//! Facts about a pair of objects that validation and resolution both need, computed once so the
//! two cannot disagree.

use std::collections::BTreeMap;

use adam_operator_api::{AgentConfig, Binary, SecretKeyRef, Tools, Volume};

/// `WORKSPACE_PLACEMENT` as the binary parses it: trimmed and lower-cased; empty is "not set".
pub fn placement(config: &AgentConfig) -> String {
    config
        .spec
        .harness
        .adam
        .coder
        .as_ref()
        .and_then(|c| c.workspace_placement.as_deref())
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// Whether the placement pins a run to the worker that first claimed it (`WORKER_ID` is then the pod
/// name, and a restart must not change it).
pub fn pins_runs(placement: &str) -> bool {
    matches!(placement, "affinity" | "isolated")
}

/// The coder's workspace volume, if the config has one.
pub fn work_volume(config: &AgentConfig) -> Option<&Volume> {
    config
        .spec
        .environment
        .volumes
        .iter()
        .find(|v| v.name == crate::contract::WORK_VOLUME)
}

/// Whether the binary is `adam-coder`.
pub fn is_coder(config: &AgentConfig) -> bool {
    config.spec.harness.adam.binary == Binary::AdamCoder
}

/// The header variables of the extra MCP servers: one per distinct Secret key, **named by the key**
/// (`SEARCH_MCP_TOKEN`), and the file refers to it with `${NAME}`. The result maps a
/// variable name to every Secret reference that wants it, so a clash is visible.
pub fn header_vars(tools: &Tools) -> BTreeMap<&str, Vec<&SecretKeyRef>> {
    let mut vars: BTreeMap<&str, Vec<&SecretKeyRef>> = BTreeMap::new();
    for server in tools.mcp_servers.values() {
        for header in server.headers.values() {
            let wanted = vars.entry(header.secret_ref.key.as_str()).or_default();
            if !wanted.contains(&&header.secret_ref) {
                wanted.push(&header.secret_ref);
            }
        }
    }
    vars
}

/// The default address of the service: `http://<name>.<namespace>.svc:8080/`.
pub fn default_public_url(name: &str, namespace: &str) -> String {
    format!("http://{name}.{namespace}.svc:{}/", crate::contract::PORT)
}
