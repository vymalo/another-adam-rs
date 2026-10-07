//! `resolve`: an `AgentService` and its `AgentConfig` into one `RuntimeSpec` with a digest, and the
//! store it needs. Every variable name, default and rule of the env contract is applied here and
//! nowhere else.

use std::collections::BTreeMap;

use adam_operator_api::{
    AgentConfig, AgentService, Binary, Coder, FsGroupChangePolicy, Github, Topology,
};
use adam_operator_ports::{
    CnpgSpec, Container, DeletionPolicy, EnvValue, EnvVar, FileSet, Mount, Network, OwnerHandle,
    PersistentVolume, Probe, ProbeAction, Probes, Resources, Role, RuntimeId, RuntimeSpec,
    SecretRef, Security, Sharing, StoreId, StoreKind, StoreSpec, VolumeSource, VolumeSpec,
    Workload, cnpg_connection,
};
use serde_json::json;

use crate::contract::{self, env, probes, sidecar};
use crate::digest::{files_hash8, spec_digest};
use crate::validate::{ConfigIssue, validate};
use crate::{convert, plan};

/// What the controller needs to reconcile one service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedAgent {
    /// The runtime's identity: the service's namespace and name.
    pub id: RuntimeId,
    /// The store's identity: the same.
    pub store_id: StoreId,
    /// What to make. `runtime.digest` is [`digest`](Self::digest).
    pub runtime: RuntimeSpec,
    /// Where the run ledger lives.
    pub store: StoreSpec,
    /// `sha256:<hex>` of what the pods run (see [`spec_digest`](crate::spec_digest)): the digest
    /// the pods are stamped with (`agents.vymalo.com/config-digest`) and
    /// `status.config.digest`, the seed of AgentRevision.
    pub digest: String,
    /// Where clients reach the agent: `spec.interfaces.a2a.publicUrl`, or
    /// `http://<name>.<namespace>.svc:8080/`. Also `PUBLIC_URL` of the processes that serve A2A.
    pub public_url: String,
}

/// Resolve a service and its config.
///
/// `owner` is the opaque handle of the service the controller got from the object; it goes into the
/// specs unread and is not part of the digest. The same input always gives the same specs and the
/// same digest: nothing here reads a clock, a random number, the environment or the cluster.
///
/// # Errors
///
/// The issues of [`validate`], when there are any: nothing is resolved from objects that are not
/// valid.
pub fn resolve(
    service: &AgentService,
    config: &AgentConfig,
    owner: OwnerHandle,
) -> Result<ResolvedAgent, Vec<ConfigIssue>> {
    validate(service, config)?;
    let name = service.metadata.name.clone().unwrap_or_default();
    let namespace = service.metadata.namespace.clone().unwrap_or_default();
    let cx = Cx::new(service, config, &name, &namespace);

    let (store_kind, database) = match (
        &service.spec.store.postgres.secret_ref,
        &service.spec.store.postgres.cnpg,
    ) {
        (Some(r), _) => {
            let r = SecretRef::new(&r.name, &r.key);
            (StoreKind::Secret(r.clone()), r)
        }
        (None, Some(c)) => (
            StoreKind::Cnpg(CnpgSpec {
                instances: u32::try_from(c.instances).unwrap_or(1),
                size: c.storage.size.0.clone(),
                storage_class: c.storage.storage_class.clone(),
            }),
            cnpg_connection(&name),
        ),
        // `validate` refused an object with neither.
        (None, None) => {
            return Err(vec![ConfigIssue {
                field: "AgentService spec.store.postgres".to_owned(),
                message: "exactly one of secretRef and cnpg".to_owned(),
            }]);
        }
    };
    let deletion = match service.spec.deletion_policy {
        adam_operator_api::DeletionPolicy::Retain => DeletionPolicy::Retain,
        adam_operator_api::DeletionPolicy::Delete => DeletionPolicy::Delete,
    };

    let (file_sets, workloads) = cx.workloads(&database);
    let (allow_from, _) = convert::peers(&service.spec.access.allow_from);
    let selects = workloads
        .iter()
        .find(|w| w.role != Role::Worker)
        .or(workloads.first())
        .map(|w| w.name.clone())
        .unwrap_or_default();

    let mut runtime = RuntimeSpec {
        owner: owner.clone(),
        deletion,
        digest: String::new(),
        suspend: service.spec.suspend,
        workloads,
        file_sets,
        network: Network {
            port: contract::PORT,
            selects,
            allow_from,
        },
    };
    let digest = spec_digest(&runtime);
    runtime.digest.clone_from(&digest);

    Ok(ResolvedAgent {
        id: RuntimeId::new(&namespace, &name),
        store_id: StoreId::new(&namespace, &name),
        runtime,
        store: StoreSpec {
            owner,
            deletion,
            kind: store_kind,
        },
        digest,
        public_url: cx.public_url,
    })
}

fn lit(name: &str, value: impl Into<String>) -> EnvVar {
    EnvVar::literal(name, value)
}

/// What every step of the resolution reads.
struct Cx<'a> {
    service: &'a AgentService,
    config: &'a AgentConfig,
    name: &'a str,
    coder: Option<&'a Coder>,
    placement: String,
    public_url: String,
    /// The agent is `adam-coder` (it has the coder's variables and a workspace).
    is_coder: bool,
}

impl<'a> Cx<'a> {
    fn new(
        service: &'a AgentService,
        config: &'a AgentConfig,
        name: &'a str,
        namespace: &str,
    ) -> Self {
        let configured = service
            .spec
            .interfaces
            .a2a
            .public_url
            .as_deref()
            .filter(|u| !u.is_empty());
        Self {
            service,
            config,
            name,
            coder: config.spec.harness.adam.coder.as_ref(),
            placement: plan::placement(config),
            public_url: configured
                .map_or_else(|| plan::default_public_url(name, namespace), str::to_owned),
            is_coder: config.spec.harness.adam.binary == Binary::AdamCoder,
        }
    }

    /// The file sets and the workloads: the workers (or the one process that is everything) first,
    /// then the front of `split`.
    fn workloads(&self, database: &SecretRef) -> (Vec<FileSet>, Vec<Workload>) {
        let adam = &self.config.spec.harness.adam;
        let mut file_sets = Vec::new();

        // The folder: inline files become an immutable, content-named set; a ConfigMap of the
        // user's is mounted as it is.
        let folder_volume = adam.agent.folder.as_ref().map(|f| {
            let source = match (&f.files, &f.config_map_ref) {
                (Some(files), _) => {
                    let set = format!("{}-agent-{}", self.name, files_hash8(files));
                    file_sets.push(FileSet {
                        name: set.clone(),
                        files: files.clone(),
                        immutable: true,
                    });
                    VolumeSource::Files {
                        file_set: set,
                        mode: contract::FILES_MODE,
                    }
                }
                (None, Some(r)) => VolumeSource::ExternalFiles {
                    name: r.name.clone(),
                    mode: contract::FILES_MODE,
                },
                (None, None) => VolumeSource::ExternalFiles {
                    name: String::new(),
                    mode: contract::FILES_MODE,
                },
            };
            VolumeSpec {
                name: contract::AGENT_FOLDER_VOLUME.to_owned(),
                source,
            }
        });

        // The extra MCP servers: one file of `${VAR}` references, never a value.
        let mcp_volume = if self.config.spec.tools.mcp_servers.is_empty() {
            None
        } else {
            let set = format!("{}-mcp", self.name);
            let mut text = serde_json::to_string_pretty(&self.mcp_json()).unwrap_or_default();
            text.push('\n');
            file_sets.push(FileSet {
                name: set.clone(),
                files: BTreeMap::from([(contract::EXTRA_MCP_FILE_NAME.to_owned(), text)]),
                immutable: false,
            });
            Some(VolumeSpec {
                name: contract::EXTRA_MCP_VOLUME.to_owned(),
                source: VolumeSource::Files {
                    file_set: set,
                    mode: contract::FILES_MODE,
                },
            })
        };

        let scaling = &self.service.spec.scaling;
        let workers = u32::try_from(scaling.workers).unwrap_or(1);
        let mut workloads = Vec::new();
        match scaling.topology {
            Topology::Combined => workloads.push(self.workload(
                Role::All,
                self.name.to_owned(),
                workers,
                database,
                folder_volume.as_ref(),
                mcp_volume.as_ref(),
            )),
            Topology::Split => {
                workloads.push(self.workload(
                    Role::Worker,
                    self.name.to_owned(),
                    workers,
                    database,
                    folder_volume.as_ref(),
                    mcp_volume.as_ref(),
                ));
                let replicas =
                    u32::try_from(scaling.front.as_ref().map_or(1, |f| f.replicas)).unwrap_or(1);
                workloads.push(self.workload(
                    Role::ControlPlane,
                    format!("{}-front", self.name),
                    replicas,
                    database,
                    folder_volume.as_ref(),
                    None,
                ));
            }
        }
        (file_sets, workloads)
    }

    #[allow(clippy::too_many_arguments)]
    fn workload(
        &self,
        role: Role,
        name: String,
        replicas: u32,
        database: &SecretRef,
        folder: Option<&VolumeSpec>,
        mcp: Option<&VolumeSpec>,
    ) -> Workload {
        let spec = &self.config.spec;
        let runs_workers = role.runs_workers();
        let image = spec.environment.image.reference.clone();

        // Volumes and mounts. A control plane serves the card and starts runs: it has the folder
        // (every role reads it) and nothing else.
        let mut volumes: Vec<VolumeSpec> = Vec::new();
        let mut mounts: Vec<Mount> = Vec::new();
        if runs_workers {
            for v in &spec.environment.volumes {
                volumes.push(VolumeSpec {
                    name: v.name.clone(),
                    source: VolumeSource::Persistent(PersistentVolume {
                        size: v.source.persistent.size.0.clone(),
                        storage_class: v.source.persistent.storage_class.clone(),
                        sharing: if v.source.persistent.per_replica {
                            Sharing::PerReplica
                        } else {
                            Sharing::Shared
                        },
                    }),
                });
                mounts.push(Mount {
                    volume: v.name.clone(),
                    path: v.mount_path.clone(),
                    read_only: false,
                });
            }
        }
        if let Some(f) = folder {
            volumes.push(f.clone());
            mounts.push(Mount {
                volume: f.name.clone(),
                path: contract::AGENT_DIR.to_owned(),
                read_only: true,
            });
        }
        if runs_workers {
            if let Some(m) = mcp {
                volumes.push(m.clone());
                mounts.push(Mount {
                    volume: m.name.clone(),
                    path: contract::EXTRA_MCP_DIR.to_owned(),
                    read_only: true,
                });
            }
            if let Some(app) = self.github_app() {
                volumes.push(VolumeSpec {
                    name: contract::GITHUB_APP_VOLUME.to_owned(),
                    source: VolumeSource::SecretFile {
                        secret: SecretRef::new(
                            &app.private_key_secret_ref.name,
                            &app.private_key_secret_ref.key,
                        ),
                        file: contract::GITHUB_APP_KEY_FILE.to_owned(),
                        mode: contract::KEY_MODE,
                    },
                });
                mounts.push(Mount {
                    volume: contract::GITHUB_APP_VOLUME.to_owned(),
                    path: contract::GITHUB_APP_DIR.to_owned(),
                    read_only: true,
                });
            }
        }

        let env = self.env(role, database);
        let stable_identity = volumes.iter().any(|v| {
            matches!(&v.source, VolumeSource::Persistent(p) if p.sharing == Sharing::PerReplica)
        }) || env.iter().any(|e| e.value == EnvValue::PodName);

        let http = |period, timeout, failures| Probe {
            action: ProbeAction::Http {
                path: contract::HEALTHZ.to_owned(),
            },
            period_secs: period,
            timeout_secs: timeout,
            failure_threshold: failures,
        };
        let resources = if runs_workers {
            convert::resources(spec.environment.resources.as_ref()).0
        } else {
            Resources {
                requests: BTreeMap::from([
                    ("cpu".to_owned(), contract::front::REQUEST_CPU.to_owned()),
                    (
                        "memory".to_owned(),
                        contract::front::REQUEST_MEMORY.to_owned(),
                    ),
                ]),
                limits: BTreeMap::from([(
                    "memory".to_owned(),
                    contract::front::LIMIT_MEMORY.to_owned(),
                )]),
            }
        };
        let container = Container {
            name: contract::AGENT_CONTAINER.to_owned(),
            image: image.clone(),
            command: if self.is_coder {
                Vec::new()
            } else {
                contract::ADAM_AGENT_COMMAND.map(str::to_owned).to_vec()
            },
            args: Vec::new(),
            env,
            port: Some(contract::PORT),
            mounts,
            probes: Probes {
                startup: Some(http(
                    probes::STARTUP_PERIOD,
                    None,
                    Some(probes::STARTUP_FAILURES),
                )),
                liveness: Some(http(
                    probes::LIVENESS_PERIOD,
                    Some(probes::LIVENESS_TIMEOUT),
                    Some(probes::LIVENESS_FAILURES),
                )),
                readiness: Some(http(
                    probes::READINESS_PERIOD,
                    Some(probes::READINESS_TIMEOUT),
                    None,
                )),
            },
            resources,
        };

        // The GitHub MCP server: a native sidecar of every process that runs workers, on loopback.
        let sidecars = match &spec.tools.github_mcp {
            Some(g) if g.sidecar && runs_workers => {
                let port = u16::try_from(g.port).unwrap_or(sidecar::DEFAULT_PORT);
                let (command, args) = sidecar::command_and_args(port);
                let env = g
                    .host
                    .as_deref()
                    .filter(|h| !h.is_empty())
                    .map(|h| vec![EnvVar::literal(env::GITHUB_HOST, h)])
                    .unwrap_or_default();
                vec![Container {
                    name: sidecar::NAME.to_owned(),
                    image,
                    command,
                    args,
                    env,
                    port: None,
                    mounts: Vec::new(),
                    probes: Probes {
                        startup: Some(Probe {
                            action: ProbeAction::Exec {
                                command: sidecar::startup_probe_command(port),
                            },
                            period_secs: sidecar::STARTUP_PERIOD,
                            timeout_secs: None,
                            failure_threshold: Some(sidecar::STARTUP_FAILURES),
                        }),
                        liveness: None,
                        readiness: None,
                    },
                    resources: Resources {
                        requests: BTreeMap::from([
                            ("cpu".to_owned(), sidecar::REQUEST_CPU.to_owned()),
                            ("memory".to_owned(), sidecar::REQUEST_MEMORY.to_owned()),
                        ]),
                        limits: BTreeMap::from([(
                            "memory".to_owned(),
                            sidecar::LIMIT_MEMORY.to_owned(),
                        )]),
                    },
                }]
            }
            _ => Vec::new(),
        };

        let sec = &spec.security;
        let front_replicas = role == Role::ControlPlane && replicas > 1;
        Workload {
            name,
            role,
            replicas,
            stable_identity,
            container,
            sidecars,
            volumes,
            security: Security {
                run_as_user: sec.run_as_user.unwrap_or(contract::DEFAULT_ID),
                run_as_group: sec.run_as_group.unwrap_or(contract::DEFAULT_ID),
                fs_group: sec.fs_group.unwrap_or(contract::DEFAULT_ID),
                fs_group_change_policy: sec.fs_group_change_policy.map(|p| match p {
                    FsGroupChangePolicy::OnRootMismatch => {
                        adam_operator_ports::FsGroupChangePolicy::OnRootMismatch
                    }
                    FsGroupChangePolicy::Always => adam_operator_ports::FsGroupChangePolicy::Always,
                }),
            },
            termination_grace_secs: if runs_workers {
                spec.environment
                    .termination_grace_period_seconds
                    .and_then(|s| u32::try_from(s).ok())
            } else {
                Some(contract::front::TERMINATION_GRACE_SECS)
            },
            min_available: front_replicas.then_some(contract::front::MIN_AVAILABLE),
        }
    }

    fn github_app(&self) -> Option<&'a adam_operator_api::GithubApp> {
        match self.coder.map(|c| &c.github) {
            Some(Github { app: Some(app), .. }) => Some(app),
            _ => None,
        }
    }

    /// The environment of one process, by role. Order is fixed: the contract's variables, then
    /// `extraEnv`, then the Secret-backed ones.
    fn env(&self, role: Role, database: &SecretRef) -> Vec<EnvVar> {
        let spec = &self.config.spec;
        let runs_workers = role.runs_workers();
        let serves_a2a = role != Role::Worker;
        let mut e: Vec<EnvVar> = vec![lit(env::LISTEN_ADDR, contract::LISTEN_ADDR)];

        // Where the coder works. WORKER_ID is the pod name when runs are pinned to a worker.
        if self.is_coder && runs_workers {
            if let Some(work) = plan::work_volume(self.config) {
                e.push(lit(env::WORKSPACE_ROOT, &work.mount_path));
            }
            if !self.placement.is_empty() {
                e.push(lit(env::WORKSPACE_PLACEMENT, &self.placement));
            }
            if plan::pins_runs(&self.placement) {
                e.push(EnvVar {
                    name: env::WORKER_ID.to_owned(),
                    value: EnvValue::PodName,
                });
            }
        }
        match (self.service.spec.scaling.topology, role) {
            (Topology::Split, Role::Worker) => e.push(lit(env::ROLE, "worker")),
            (Topology::Split, Role::ControlPlane) => e.push(lit(env::ROLE, "control-plane")),
            _ => {}
        }
        if serves_a2a {
            e.push(lit(env::PUBLIC_URL, &self.public_url));
        }
        if self.config.spec.harness.adam.agent.folder.is_some() {
            e.push(lit(env::ADAM_AGENT_DIR, contract::AGENT_DIR));
        }

        if runs_workers {
            match (&spec.model.base_url.value, &spec.model.base_url.secret_ref) {
                (Some(url), _) => e.push(lit(env::MODEL_BASE_URL, url)),
                (None, Some(r)) => {
                    e.push(EnvVar::secret(
                        env::MODEL_BASE_URL,
                        SecretRef::new(&r.name, &r.key),
                    ));
                }
                (None, None) => {}
            }
            e.push(lit(env::MODEL, &spec.model.model));
            if let Some(body) = &spec.model.extra_body
                && !body.is_empty()
            {
                e.push(lit(
                    env::MODEL_EXTRA_BODY,
                    serde_json::Value::Object(body.clone()).to_string(),
                ));
            }
            if let Some(echo) = spec.model.echo_reasoning {
                e.push(lit(env::MODEL_ECHO_REASONING, echo.as_str()));
            }
            if let Some(c) = self.coder {
                if let Some(m) = &c.opencode_model {
                    e.push(lit(env::OPENCODE_MODEL, m));
                }
                for (name, value) in [
                    (env::WORKERS, c.workers.map(i64::from)),
                    (env::MAX_CHECK_CYCLES, c.max_check_cycles.map(i64::from)),
                    (env::CHECK_TIMEOUT_SECS, c.check_timeout_secs),
                    (env::WORKSPACE_SWEEP_SECS, c.workspace_sweep_secs),
                ] {
                    if let Some(n) = value {
                        e.push(lit(name, n.to_string()));
                    }
                }
            }
            // Parity with the chart, which keeps it for one release for folders written before the
            // GitHub server became a sidecar. adam-agent must refuse local processes unless its own
            // deployment opts in, so it is the coder's alone.
            if self.is_coder {
                e.push(lit(env::MCP_ALLOW_STDIO, "true"));
            }
            if !spec.tools.mcp_servers.is_empty() {
                e.push(lit(env::ADAM_EXTRA_MCP_FILE, contract::EXTRA_MCP_FILE));
            }
            if spec.tools.allow_insecure_http {
                e.push(lit(env::MCP_ALLOW_INSECURE, "true"));
            }
            if let Some(g) = spec.tools.github_mcp.as_ref().filter(|g| g.sidecar)
                && self.is_coder
            {
                e.push(lit(
                    env::GITHUB_MCP_URL,
                    format!("http://127.0.0.1:{}", g.port),
                ));
            }
            if let Some(c) = self.coder {
                if !c.allowed_repo_hosts.is_empty() {
                    e.push(lit(env::ALLOWED_REPO_HOSTS, c.allowed_repo_hosts.join(",")));
                }
                if let Some(url) = &c.github_api_url {
                    e.push(lit(env::GITHUB_API_URL, url));
                }
                if !c.create_repo_owners.is_empty() {
                    e.push(lit(env::CREATE_REPO_OWNERS, c.create_repo_owners.join(",")));
                }
                if let Some(app) = &c.github.app {
                    e.push(lit(env::GITHUB_APP_ID, app.id.trim()));
                    match (&app.installation_id, &app.owners) {
                        (Some(id), _) => {
                            e.push(lit(env::GITHUB_APP_INSTALLATION_ID, id.to_string()))
                        }
                        (None, Some(owners)) => {
                            let names: Vec<&str> = owners.iter().map(|o| o.trim()).collect();
                            e.push(lit(env::GITHUB_APP_OWNERS, names.join(",")));
                        }
                        (None, None) => {}
                    }
                    e.push(lit(
                        env::GITHUB_APP_PRIVATE_KEY_PATH,
                        format!(
                            "{}/{}",
                            contract::GITHUB_APP_DIR,
                            contract::GITHUB_APP_KEY_FILE
                        ),
                    ));
                }
                if let Some(draft) = c.pr_draft {
                    e.push(lit(env::PR_DRAFT, draft.to_string()));
                }
                if let Some(a) = &c.git_author {
                    e.push(lit(env::GIT_AUTHOR_NAME, &a.name));
                    e.push(lit(env::GIT_AUTHOR_EMAIL, &a.email));
                }
            }
        }

        for (name, value) in &spec.extra_env {
            e.push(lit(name, value));
        }
        e.push(EnvVar::secret(env::DATABASE_URL, database.clone()));
        if runs_workers {
            let key = &spec.model.api_key_secret_ref;
            e.push(EnvVar::secret(
                env::MODEL_API_KEY,
                SecretRef::new(&key.name, &key.key),
            ));
            if let Some(t) = self.coder.and_then(|c| c.github.token.as_ref()) {
                e.push(EnvVar::secret(
                    env::GITHUB_TOKEN,
                    SecretRef::new(&t.secret_ref.name, &t.secret_ref.key),
                ));
            }
            // One variable per distinct Secret key, named by the key; the file refers to it.
            for (var, wanted) in plan::header_vars(&spec.tools) {
                if let Some(r) = wanted.first() {
                    e.push(EnvVar::secret(var, SecretRef::new(&r.name, &r.key)));
                }
            }
        }
        if serves_a2a && let Some(r) = &self.service.spec.interfaces.a2a.bearer_tokens_secret_ref {
            e.push(EnvVar::secret(
                env::A2A_BEARER_TOKENS,
                SecretRef::new(&r.name, &r.key),
            ));
        }
        e
    }

    /// The extra MCP file, in the shape of `mcp.json`: `${VAR}` references only.
    fn mcp_json(&self) -> serde_json::Value {
        let mut servers = serde_json::Map::new();
        for (id, s) in &self.config.spec.tools.mcp_servers {
            let mut server = serde_json::Map::new();
            server.insert("type".to_owned(), json!("http"));
            server.insert("url".to_owned(), json!(s.url));
            if !s.headers.is_empty() {
                let headers: serde_json::Map<String, serde_json::Value> = s
                    .headers
                    .iter()
                    .map(|(name, h)| {
                        let prefix = h.prefix.as_deref().unwrap_or_default();
                        (
                            name.clone(),
                            json!(format!("{prefix}${{{}}}", h.secret_ref.key)),
                        )
                    })
                    .collect();
                server.insert("headers".to_owned(), serde_json::Value::Object(headers));
            }
            if s.optional {
                server.insert("optional".to_owned(), json!(true));
            }
            servers.insert(id.clone(), serde_json::Value::Object(server));
        }
        json!({ "mcpServers": servers })
    }
}
