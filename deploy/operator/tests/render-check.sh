#!/bin/sh
# Assertions on the rendered chart: the properties the design depends on, so a careless edit to the templates or values
# fails CI instead of widening the operator. Needs `helm`, `grep`, `awk`, `sed`, `cmp` and `diff`.
#
#   sh deploy/operator/tests/render-check.sh                    (from the repository root)
#   UPDATE_GOLDEN=1 sh deploy/operator/tests/render-check.sh    rewrites tests/golden/*.yaml, for a deliberate change
#
# Three renders are golden: the values of the Application on netcup (examples/netcup.values.yaml), the other shape those
# values do not make (tests/secret.values.yaml), and the defaults. Also checked: deploy/operator-crds against deploy/crds.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
chart="$here/.."
crds_chart="$chart/../operator-crds"
crds_file="$chart/../crds/agents.vymalo.com.yaml"
ns=another-agentic-system
out=$(mktemp)
err=$(mktemp)
fail=0
trap 'rm -f "$out" "$err"' EXIT

check() { # check <description> <command...>
  desc=$1; shift
  if "$@" >/dev/null 2>&1; then
    echo "ok   $desc"
  else
    echo "FAIL $desc"
    fail=1
  fi
}
has() { grep -Eq -- "$1" "$out"; }
lacks() { ! grep -Eq -- "$1" "$out"; }
fails() { ! "$@"; }
# render [helm args]: the chart into $out. The defaults render with no values at all.
render() { helm template adam-operator "$chart" --namespace "$ns" "$@" > "$out"; }
# refused <message fragment> [helm args]: the render fails, and says why.
refused() {
  frag=$1; shift
  if helm template adam-operator "$chart" --namespace "$ns" "$@" > /dev/null 2> "$err"; then return 1; fi
  grep -qF -- "$frag" "$err"
}
# doc <Kind> [name]: the YAML document(s) of one kind (and name) from the render.
doc() {
  awk -v k="$1" -v n="${2:-}" '
    function flush() {
      if (buf ~ ("(^|\n)kind: " k "\n") && (n == "" || buf ~ ("(^|\n)  name: " n "\n"))) printf "%s", buf
      buf = ""
    }
    /^---$/ { flush(); next }
    { buf = buf $0 "\n" }
    END { flush() }' "$out"
}
# dhas <Kind> <name> <pattern>...: the document exists and every pattern matches in it. dlacks: none does.
dhas() {
  k=$1; n=$2; shift 2
  d=$(doc "$k" "$n")
  [ -n "$d" ] || return 1
  for p in "$@"; do printf '%s\n' "$d" | grep -Eq -- "$p" || return 1; done
}
dlacks() {
  k=$1; n=$2; shift 2
  d=$(doc "$k" "$n")
  [ -n "$d" ] || return 1
  for p in "$@"; do ! printf '%s\n' "$d" | grep -Eq -- "$p" || return 1; done
}
dcount() { [ "$(doc "$1" "$2" | grep -Ec -- "$3")" -eq "$4" ]; }
# The rules of the Role, one line each: `<groups> <resources> <verbs>`, as the READMEs of the crates list them.
role_rules() {
  doc Role adam-operator | awk '
    /apiGroups:/ { g = $0; sub(/.*apiGroups: */, "", g) }
    /resources:/ { r = $0; sub(/.*resources: */, "", r) }
    /verbs:/ { v = $0; sub(/.*verbs: */, "", v); print g " " r " " v }'
}
rules_are() { [ "$(role_rules)" = "$1" ]; }

# ---- The three golden renders ---------------------------------------------------------------------------------------------
golden() { # golden <name> <helm args>: the render equals tests/golden/<name>.yaml (or rewrites it)
  name=$1; shift
  # The goldens hold the placeholder tag: the bump job rewrites values.yaml on main, and a golden must not follow it.
  render --set image.tag=sha-0000000 "$@"
  if [ "${UPDATE_GOLDEN:-}" = 1 ]; then cp "$out" "$here/golden/$name.yaml"; echo "wrote tests/golden/$name.yaml" >&2; return 0; fi
  cmp -s "$out" "$here/golden/$name.yaml"
}
check "the netcup render equals tests/golden/netcup.yaml" golden netcup -f "$chart/examples/netcup.values.yaml"
check "the render of tests/secret.values.yaml equals tests/golden/secret.yaml" golden secret -f "$here/secret.values.yaml"
check "the default render equals tests/golden/default.yaml" golden default

# ---- The default render: no registry, nothing a secret could be in --------------------------------------------------------
render
check "no Secret object is rendered (the token is an existing Secret or an ExternalSecret)" lacks '^kind: Secret$'
check "no token-looking value in the render" lacks '(ghp_|github_pat_|gho_|sk-[A-Za-z0-9]{8}|-----BEGIN|AKIA[0-9A-Z]{16}|xox[bp]-|eyJ[A-Za-z0-9_-]{20})'
check "no ClusterRole and no ClusterRoleBinding: the operator is namespaced" lacks '^kind: Cluster(Role|RoleBinding)$'
check "no Ingress, ServiceMonitor or CRD: this chart is the operator only" lacks '^kind: (Ingress|ServiceMonitor|CustomResourceDefinition)$'
check "no token, no registry: no REGISTRY_TOKEN_FILE, no registry port, no registry Service" \
  lacks 'REGISTRY_TOKEN_FILE|name: registry|adam-operator-registry'
check "no token: the pod mounts no volume" dlacks Deployment adam-operator '^      volumes:'
check "no token: ingress is closed, not open (ingress: [])" dhas NetworkPolicy adam-operator '^  ingress: \[\]$'
check "the metrics Service is there" dhas Service adam-operator-metrics 'port: 9090'

# ---- The Deployment -----------------------------------------------------------------------------------------------------
check "one replica" dhas Deployment adam-operator '^  replicas: 1$'
check "strategy Recreate (never two reconcilers at once)" dhas Deployment adam-operator '^    type: Recreate$'
check "the image is a sha-<7 hex> tag of the operator package" \
  dhas Deployment adam-operator 'image: "ghcr.io/vymalo/another-adam-rs/operator:sha-[0-9a-f]{7}"'
check "the container runs \`run\`" dhas Deployment adam-operator '^          args: \[run\]$'
check "WATCH_NAMESPACE is the release's namespace" dhas Deployment adam-operator "value: \"$ns\""
check "POD_NAME is the pod's name (the reporting instance of Events)" dhas Deployment adam-operator 'fieldPath: metadata.name'
check "probes: /healthz twice (startup, liveness) and /readyz once, on the health port 8081" \
  dcount Deployment adam-operator 'path: /healthz' 2
check "the readiness probe is /readyz" dhas Deployment adam-operator 'path: /readyz'
check "three probes on the named port health, which is 8081" dcount Deployment adam-operator 'port: health' 3
check "health is 8081, metrics 9090" dhas Deployment adam-operator 'containerPort: 8081'
check "the pod is non-root, uid 65532" dhas Deployment adam-operator 'runAsUser: 65532'
check "the pod has the RuntimeDefault seccomp profile" dhas Deployment adam-operator 'type: RuntimeDefault'
check "the container has no privilege escalation" dhas Deployment adam-operator 'allowPrivilegeEscalation: false'
check "the container's root filesystem is read-only" dhas Deployment adam-operator 'readOnlyRootFilesystem: true'
check "the container drops ALL capabilities" dhas Deployment adam-operator 'drop: \[ALL\]'
check "no hostNetwork, hostPID, hostIPC, hostPath or privileged" lacks '(hostNetwork|hostPID|hostIPC|hostPath|privileged: true)'
check "resources have defaults: a CPU request and a memory limit" dhas Deployment adam-operator 'cpu: 50m'
check "the pod gets a service account token (its way in)" dhas Deployment adam-operator 'automountServiceAccountToken: true'

# ---- RBAC: what the READMEs of the crates list, and nothing else ----------------------------------------------------------
base_rules='[agents.vymalo.com] [agentservices, agentconfigs] [get, list, watch, patch]
[agents.vymalo.com] [agentservices/status, agentconfigs/status] [patch]
[events.k8s.io] [events] [create, patch]
[apps] [statefulsets, deployments] [get, list, watch, patch, create, delete]
[""] [services, configmaps, persistentvolumeclaims] [get, list, patch, create, delete]
[networking.k8s.io] [networkpolicies] [get, list, patch, create, delete]
[policy] [poddisruptionbudgets] [get, list, patch, create, delete]
[""] [pods] [list, watch]'
cnpg_rule='[postgresql.cnpg.io] [clusters] [get, patch, create, delete]'
check "the Role has exactly the rules of the controller's, the runtime's and the store's READMEs (storeCnpg on by default)" \
  rules_are "$base_rules
$cnpg_rule"
check "the Role is in the release's namespace" dhas Role adam-operator "namespace: $ns"
check "and so is its binding" dhas RoleBinding adam-operator "namespace: $ns"
check "no right on Secrets, none on a wildcard, none on a CustomResourceDefinition, a node or a namespace" \
  dlacks Role adam-operator 'secrets' '\*' 'customresourcedefinitions' 'nodes' 'namespaces'
check "the binding is to the operator's own service account" dhas RoleBinding adam-operator 'kind: ServiceAccount' 'name: adam-operator$'
render --set storeCnpg=false
check "storeCnpg=false: no right on CloudNativePG" rules_are "$base_rules"
check "storeCnpg=false: no mention of CloudNativePG" lacks 'cnpg'
render --set serviceAccount.create=false --set serviceAccount.name=elsewhere
check "serviceAccount.create=false: no ServiceAccount is made" lacks '^kind: ServiceAccount$'
check "serviceAccount.create=false: the binding and the pod name the given one" \
  dhas RoleBinding adam-operator 'name: elsewhere$'
check "serviceAccount.create=false: the pod runs as it" dhas Deployment adam-operator 'serviceAccountName: elsewhere'

# ---- The registry: only with a token --------------------------------------------------------------------------------------
render -f "$here/secret.values.yaml"
check "a token Secret: the registry Service is ClusterIP on 8080" dhas Service adam-operator-registry 'type: ClusterIP' 'port: 8080'
check "a token Secret: the token is a file, mounted read-only, from the Secret and the key named" \
  dhas Deployment adam-operator 'value: /var/run/secrets/adam-operator/registry/token' 'secretName: adam-operator-registry-token' 'key: bearer' 'readOnly: true' \
  'path: token'
check "a token Secret: the file is readable by the group only (0440), and the pod has that group" \
  dhas Deployment adam-operator 'defaultMode: 288' 'fsGroup: 65532'
check "the token is never an environment variable" lacks 'REGISTRY_TOKEN$'
check "registry.publicUrl is REGISTRY_PUBLIC_URL" dhas Deployment adam-operator 'https://registry.example.com/registry/v1/agents'
subject_in_release_ns() { doc RoleBinding adam-operator | grep -A3 'kind: ServiceAccount' | grep -q "namespace: $ns"; }
check "WATCH_NAMESPACE follows watchNamespace" dhas Deployment adam-operator 'value: "agents"'
check "the Role and the binding are in the watched namespace" dhas Role adam-operator 'namespace: agents'
check "the binding too" dhas RoleBinding adam-operator 'namespace: agents'
check "the subject stays the release's service account" subject_in_release_ns
check "tuning keys are the variables of \`run\`" \
  dhas Deployment adam-operator 'ADAM_OPERATOR_CONCURRENCY' 'ADAM_OPERATOR_RESYNC_SECS' 'ADAM_OPERATOR_RESYNC_PENDING_SECS'
check "imagePullSecrets are passed on" dhas Deployment adam-operator 'name: ghcr-pull'
check "the NetworkPolicy has Ingress and Egress, and ingress is the registry's peers and the scrapers, on their ports" \
  dhas NetworkPolicy adam-operator '^    - Ingress$' '^    - Egress$' 'port: 8080' 'port: 9090'
check "ingress has two rules" dcount NetworkPolicy adam-operator '^    - from:$' 2
check "egress is the API server's port only, to the address given" dhas NetworkPolicy adam-operator 'port: 6443' 'cidr: 10.0.0.10/32'
check "and not 443" dlacks NetworkPolicy adam-operator 'port: 443$'
render --set registry.tokenSecret.name=t --set 'networkPolicy.registry.allowFrom[0].podSelector.matchLabels.a=b'
check "egress, by default, is 443 and 6443, to any address" dhas NetworkPolicy adam-operator 'port: 443$' 'port: 6443$'
check "and no ipBlock" dlacks NetworkPolicy adam-operator 'ipBlock'
render --set registry.tokenSecret.name=t --set networkPolicy.enabled=false
check "networkPolicy.enabled=false: no NetworkPolicy, and then a registry needs no allowFrom" lacks '^kind: NetworkPolicy$'

# ---- The ExternalSecret --------------------------------------------------------------------------------------------------
render -f "$chart/examples/netcup.values.yaml"
check "netcup: an ExternalSecret on the ssegning-aws ClusterSecretStore, from prod/another-agentic/env, one property" \
  dhas ExternalSecret adam-operator-registry 'name: ssegning-aws' 'kind: ClusterSecretStore' 'key: prod/another-agentic/env' 'property: agent_registry_token'
check "netcup: the Secret it makes is the one the pod mounts, key token" \
  dhas ExternalSecret adam-operator-registry 'name: adam-operator-registry$' 'secretKey: token'
check "netcup: the pod mounts it" dhas Deployment adam-operator 'secretName: adam-operator-registry' 'key: token'
check "netcup: only the orchestrator's pods may read the registry" \
  dhas NetworkPolicy adam-operator 'app.kubernetes.io/component: orchestrator'
check "netcup: no Secret object, still" lacks '^kind: Secret$'

# ---- What is refused -------------------------------------------------------------------------------------------------------
check "a replica count is refused (no leader election in v0)" refused 'no leader election' --set replicaCount=2
check "an empty image tag is refused" refused 'image.tag is required' --set image.tag=
check "the latest tag is refused" refused 'must not be latest' --set image.tag=latest
check "a malformed digest is refused" refused 'image.digest must be sha256' --set image.digest=abc
check "a watchNamespace that is not one namespace name is refused (watchNamespace is one name)" refused 'watchNamespace must be one namespace' --set-string 'watchNamespace=A b'
check "a token Secret and an ExternalSecret together are refused" \
  refused 'the token comes from one place' --set registry.tokenSecret.name=t --set externalSecrets.enabled=true
check "a registry with no one allowed to read it is refused" refused 'networkPolicy.registry.allowFrom is required' --set registry.tokenSecret.name=t
check "a peer that names nothing is refused" refused 'a peer needs a namespaceSelector' \
  --set registry.tokenSecret.name=t --set 'networkPolicy.registry.allowFrom[0].foo=bar'
check "an ExternalSecret with no property is refused" \
  refused 'externalSecrets.properties.registryToken is required' --set externalSecrets.enabled=true --set externalSecrets.properties.registryToken= \
  --set networkPolicy.enabled=false
check "an ExternalSecret store of another kind is refused" refused 'must be ClusterSecretStore or SecretStore' \
  --set externalSecrets.enabled=true --set externalSecrets.secretStoreRef.kind=Other --set networkPolicy.enabled=false
check "a registry.publicUrl with credentials is refused" refused 'registry.publicUrl must be an absolute http(s) URL' --set 'registry.publicUrl=http://u:p@x/'
check "no API server port is refused" refused 'networkPolicy.apiServer.ports must name at least one port' --set 'networkPolicy.apiServer.ports=null'
check "a tuning key that is not a setting is refused" refused 'is not a setting' --set tuning.foo=1
check "a tuning value below 1 is refused" refused 'at least 1' --set tuning.concurrency=0
check "storeCnpg must be a boolean" refused 'storeCnpg must be true or false' --set storeCnpg=yes
check "a missing service account name is refused when none is created" refused 'serviceAccount.name is required' --set serviceAccount.create=false

# ---- The CRDs chart: one source ------------------------------------------------------------------------------------------
check "deploy/operator-crds/files is a copy of deploy/crds (cp deploy/crds/agents.vymalo.com.yaml deploy/operator-crds/files/)" \
  cmp -s "$crds_file" "$crds_chart/files/agents.vymalo.com.yaml"
# What the chart prints, without the `# Source:` lines and the separators and header comments Helm moves around, is the file.
strip() { grep -Ev '^(# Source: |---$|# Generated by |# CI fails when )' "$1"; }
crds_equal() {
  helm template adam-operator-crds "$crds_chart" --namespace "$ns" > "$err" || return 1
  [ "$(strip "$err" | cmp - "$(strip "$crds_file" > "$out" && echo "$out")" 2>&1; echo $?)" = 0 ]
}
check "the CRDs chart renders exactly deploy/crds/agents.vymalo.com.yaml" crds_equal
crds_kinds() {
  helm template adam-operator-crds "$crds_chart" --namespace "$ns" > "$err" || return 1
  [ "$(grep -c '^kind: CustomResourceDefinition$' "$err")" -eq 2 ] && [ "$(grep -c '^  name: .*\.agents\.vymalo\.com$' "$err")" -eq 2 ] \
    && ! grep -Eq '^kind: (Deployment|Role|Secret)$' "$err"
}
check "it holds the two CRDs and nothing else" crds_kinds

if [ "$fail" -eq 0 ]; then echo "operator chart checks passed"; else echo "operator chart checks FAILED"; exit 1; fi
