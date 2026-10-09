#!/bin/sh
# The ${VAR} of mcp.json, in the checks of the extra MCP servers, are text, not expansions.
# shellcheck disable=SC2016
# Assertions on the rendered chart: the properties the design depends on, so a
# careless edit to the templates or values fails CI instead of exposing the
# coder. Needs `helm`, `grep` and `awk`, and `jq` for the extra MCP servers' file.
#
#   deploy/coder/tests/render-check.sh            (from the repository root)
set -eu

chart="$(dirname "$0")/.."
out=$(mktemp)
vals=$(mktemp)
trap 'rm -f "$out" "$vals"' EXIT
fail=0

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
count() { [ "$(grep -Ec -- "$1" "$out")" -eq "$2" ]; }
fails() { ! "$@"; }
# doc <Kind> [file]: the YAML document(s) of one kind from the render (or a file).
doc() {
  awk -v k="$1" '
    function flush() { if (buf ~ ("(^|\n)kind: " k "\n")) printf "%s", buf; buf = "" }
    /^---$/ { flush(); next }
    { buf = buf $0 "\n" }
    END { flush() }' "${2:-$out}"
}
dhas() { doc "$1" | grep -Eq -- "$2"; }
dlacks() { ! doc "$1" | grep -Eq -- "$2"; }
dcount() { [ "$(doc "$1" | grep -Ec -- "$2")" -eq "$3" ]; }
# The parts of a StatefulSet that must not change between topologies: its
# name, the immutable selector and the volume claim (so the PVC is reused).
sts_identity() { # sts_identity <file>
  doc StatefulSet "$1" | awk '
    /^  name: / && !n { print; n = 1 }
    /^  serviceName:/ { print }
    /^  selector:/ { on = 1 } /^  template:/ { on = 0 }
    /^  volumeClaimTemplates:/ { on = 1 }
    on { print }'
}

helm template coder "$chart" --namespace coder-ns --set image.tag=sha-abc1234 > "$out"

golden="$chart/tests/golden/combined.yaml"
# The default render is the pre-split chart, byte for byte (regenerate with the
# command above, at the same --namespace and --set, only for a deliberate change).
check "the default render equals tests/golden/combined.yaml" cmp -s "$out" "$golden"

# The model's reasoning (ADR 0020 of adam-rs): MODEL_EXTRA_BODY is a value of the chart, not a secret,
# empty by default (nothing is rendered), and a JSON object when it is set. MODEL_ECHO_REASONING likewise.
check "by default no MODEL_EXTRA_BODY and no MODEL_ECHO_REASONING are rendered" \
  lacks 'MODEL_EXTRA_BODY|MODEL_ECHO_REASONING'
helm_reasoning() { helm template coder "$chart" --namespace coder-ns --set image.tag=sha-abc1234 "$@"; }
helm_reasoning --set config.modelExtraBody.reasoning_effort=medium --set config.modelEchoReasoning=reasoning_content > "$out"
check "a map becomes MODEL_EXTRA_BODY as compact JSON, in the StatefulSet's env" \
  dhas StatefulSet 'name: MODEL_EXTRA_BODY'
check "... with the JSON as its value" dhas StatefulSet 'value: "\{\\"reasoning_effort\\":\\"medium\\"\}"'
check "... and it is a plain value, never a secret reference" \
  dlacks Secret 'MODEL_EXTRA_BODY'
check "config.modelEchoReasoning becomes MODEL_ECHO_REASONING" \
  dhas StatefulSet 'value: "reasoning_content"'
printf 'config:\n  modelExtraBody: |\n    {"thinking": {"type": "enabled"}}\n' > "$vals"
helm_reasoning -f "$vals" > "$out"
check "a JSON string is taken as written" dhas StatefulSet 'name: MODEL_EXTRA_BODY'
helm_reasoning -f "$vals" --set config.role=control-plane > "$out"
check "a control plane runs no model: it renders neither variable" lacks 'MODEL_EXTRA_BODY|MODEL_ECHO_REASONING'
for bad in '{oops' '[1]' '"text"' '{"model":"x"}' '{"stream":true}'; do
  printf 'config:\n  modelExtraBody: %s\n' "'$bad'" > "$vals"
  check "modelExtraBody $bad is refused" fails helm_reasoning -f "$vals"
done
printf 'config:\n  modelExtraBody: |\n    {"thinking": {"type": "enabled"}}\n' > "$vals"
check "modelExtraBody and extraEnv MODEL_EXTRA_BODY together are refused" \
  fails helm_reasoning -f "$vals" --set-string config.extraEnv.MODEL_EXTRA_BODY={}
check "modelEchoReasoning other than reasoning_content or reasoning is refused" \
  fails helm_reasoning --set config.modelEchoReasoning=yes
check "modelEchoReasoning and extraEnv MODEL_ECHO_REASONING together are refused" \
  fails helm_reasoning --set config.modelEchoReasoning=reasoning_content --set-string config.extraEnv.MODEL_ECHO_REASONING=reasoning
# The model's context window (ADR 0032 of adam-rs): MODEL_CONTEXT_WINDOW, a count of tokens, nothing by default.
# envval <name> <value>: the StatefulSet sets the variable to that literal.
envval() { doc StatefulSet | grep -A1 -- "- name: $1\$" | grep -Fq -- "value: \"$2\""; }
helm_reasoning > "$out"
check "by default no MODEL_CONTEXT_WINDOW is rendered" lacks 'MODEL_CONTEXT_WINDOW'
for window in 1000000 9007199254740991 '"131072"'; do
  printf 'config:\n  modelContextWindow: %s\n' "$window" > "$vals"
  helm_reasoning -f "$vals" > "$out"
  # Helm reads the numbers of a values file as floats: 1000000 must not become 1e+06.
  check "modelContextWindow $window from a values file is MODEL_CONTEXT_WINDOW in full" \
    envval MODEL_CONTEXT_WINDOW "$(printf '%s' "$window" | tr -d '"')"
done
helm_reasoning --set config.modelContextWindow=131072 > "$out"
check "... and from --set" envval MODEL_CONTEXT_WINDOW 131072
helm_reasoning --set config.modelContextWindow=131072 --set config.role=control-plane > "$out"
check "a control plane runs no model: no MODEL_CONTEXT_WINDOW" lacks 'MODEL_CONTEXT_WINDOW'
helm_reasoning --set config.modelContextWindow=131072 --set topology=split > "$out"
check "split: the worker StatefulSet has it" envval MODEL_CONTEXT_WINDOW 131072
check "split: the front has not" dlacks Deployment 'MODEL_CONTEXT_WINDOW'
for bad in 0 -1 1.5 9007199254740992 lots true; do
  printf 'config:\n  modelContextWindow: %s\n' "$bad" > "$vals"
  check "modelContextWindow $bad is refused" fails helm_reasoning -f "$vals"
done
check "modelContextWindow and extraEnv MODEL_CONTEXT_WINDOW together are refused" \
  fails helm_reasoning --set config.modelContextWindow=131072 --set-string config.extraEnv.MODEL_CONTEXT_WINDOW=131072
# The checks below read the default render again.
helm_reasoning > "$out"

# Local-process MCP servers are the coder's alone to allow (ADR 0009, decision 8): the image sets nothing.
# The shipped mcp.json no longer starts one (the GitHub MCP server is a sidecar, ADR 0017, D4), but the worker
# keeps the variable for one release, for a vendored agent folder that still does.
check "the worker keeps allowing local-process MCP servers for one release" count 'name: MCP_ALLOW_STDIO' 1

check "never exposed: no Ingress, Route or Gateway" lacks '^kind: (Ingress|IngressRoute|HTTPRoute|Gateway)$'
check "never exposed: no LoadBalancer or NodePort" lacks 'type: (LoadBalancer|NodePort)'
check "exactly one Service, and it is ClusterIP" count '^kind: Service$' 1
check "the Service is ClusterIP" has '^  type: ClusterIP$'
check "a NetworkPolicy exists" has '^kind: NetworkPolicy$'
check "ingress only from the another-agentic-system namespace" has 'kubernetes.io/metadata.name: another-agentic-system'
check "the NetworkPolicy restricts ingress only (egress stays open)" lacks '^    - Egress$'
check "one StatefulSet with one replica" count '^kind: StatefulSet$' 1
check "one replica" has '^  replicas: 1$'
check "fsGroupChangePolicy is OnRootMismatch" has 'fsGroupChangePolicy: OnRootMismatch'
check "runs as uid 10001, non-root" has 'runAsUser: 10001'
check "the PVC is on longhorn and mounted at /work" has 'storageClassName: "longhorn"'
check "/work is the workspace root" has 'mountPath: /work'
grace() { # the pod gets at least 60 s to finish in-flight steps after SIGTERM
  awk '/terminationGracePeriodSeconds:/ { print ($2 >= 60) ? "ok" : "short"; found = 1 } END { if (!found) print "missing" }' "$out" | grep -qx ok
}
check "terminationGracePeriodSeconds is at least 60" grace
check "liveness, readiness and startup probes use /healthz" count 'path: /healthz' 3
check "a CNPG Cluster exists" has '^kind: Cluster$'
check "DATABASE_URL comes from the CNPG app secret" has 'name: coder-db-app'
check "an ExternalSecret reads from ssegning-aws" has 'name: ssegning-aws'
check "the ExternalSecret key is prod/meta/test-app" has 'key: prod/meta/test-app'
check "no Secret object is rendered (no plaintext secrets)" lacks '^kind: Secret$'
check "no token-looking value in the render" lacks '(ghp_|github_pat_|sk-[A-Za-z0-9]{8})'
check "the image tag comes from values" has 'image: "ghcr.io/vymalo/another-adam-rs/coder:sha-abc1234"'
check "ROLE is not rendered by default (the binary runs all)" lacks 'name: ROLE'
check "the agent card URL is the in-cluster Service" has 'http://coder.coder-ns.svc.cluster.local:8080/'

check "repositories are restricted to github.com by default" has 'name: ALLOWED_REPO_HOSTS'
check "the default allowlist is exactly github.com" has 'value: "github.com"'
check "local repositories are never enabled by the chart" lacks 'ALLOW_LOCAL_REPOS'
check "the GitHub API is api.github.com by default" has 'value: "https://api.github.com"'

# Overrides take effect (values-driven, not hard-coded).
helm template coder "$chart" --namespace coder-ns \
  --set externalSecrets.properties.githubToken=my_prop \
  --set networkPolicy.allowedNamespace=orchestrator \
  --set persistence.storageClass=fast > "$out"
check "ExternalSecret property names are values-driven" has 'property: my_prop'
check "the allowed namespace is values-driven" has 'kubernetes.io/metadata.name: orchestrator'
check "the storage class is values-driven" has 'storageClassName: "fast"'

helm template coder "$chart" --namespace coder-ns \
  --set 'config.allowedRepoHosts={github.com,ghe.example.com:8443}' \
  --set config.githubApiUrl=https://ghe.example.com/api/v3 > "$out"
check "the repository allowlist is values-driven and comma-joined" has 'value: "github.com,ghe.example.com:8443"'
check "the GitHub API URL is values-driven" has 'value: "https://ghe.example.com/api/v3"'

check "creating repositories is off by default: no CREATE_REPO_OWNERS" lacks 'name: CREATE_REPO_OWNERS'
helm template coder "$chart" --namespace coder-ns \
  --set 'github.createRepoOwners={acme,scratch}' > "$out"
check "the owners a repository may be created for are values-driven and comma-joined" has 'value: "acme,scratch"'
check "CREATE_REPO_OWNERS is rendered once" count 'name: CREATE_REPO_OWNERS' 1
helm template coder "$chart" --namespace coder-ns --set config.role=control-plane \
  --set 'github.createRepoOwners={acme}' > "$out"
check "a control plane gets no CREATE_REPO_OWNERS" lacks 'name: CREATE_REPO_OWNERS'

helm template coder "$chart" --namespace coder-ns --set config.role=worker > "$out"
check "ROLE is rendered when config.role is set" has 'name: ROLE'
check "ROLE carries the value" has 'value: "worker"'
check "the probes still use /healthz for a worker" count 'path: /healthz' 3

# The model and GitHub settings belong to the roles that run workers. A control plane only starts,
# delivers to, cancels and views runs, so it gets none of them (and needs neither secret).
model_and_github='name: (MODEL_API_KEY|GITHUB_TOKEN|MODEL_BASE_URL|MODEL|OPENCODE_MODEL)$'
workspace_and_checks='name: (WORKSPACE_ROOT|WORKSPACE_SWEEP_SECS|WORKERS|MAX_CHECK_CYCLES|CHECK_TIMEOUT_SECS|ALLOWED_REPO_HOSTS|GITHUB_API_URL|PR_DRAFT|GIT_AUTHOR_NAME|GIT_AUTHOR_EMAIL)$'
secrets_of_workers='secretKey: (MODEL_API_KEY|GITHUB_TOKEN)$'
front='name: (A2A_BEARER_TOKENS|DATABASE_URL|PUBLIC_URL)$'

helm template coder "$chart" --namespace coder-ns --set config.role=control-plane > "$out"
check "a control plane renders ROLE=control-plane" has 'value: "control-plane"'
check "a control plane gets no model, GitHub or OpenCode settings" lacks "$model_and_github"
check "a control plane gets no workspace or check settings" lacks "$workspace_and_checks"
check "a control plane connects no MCP server, so it allows none: no MCP_ALLOW_STDIO" lacks 'name: MCP_ALLOW_STDIO'
check "a control plane's ExternalSecret carries neither worker secret" lacks "$secrets_of_workers"
check "a control plane keeps the A2A tokens, the database and the public URL" count "$front" 3
check "a control plane's ExternalSecret still has the A2A tokens" has 'secretKey: A2A_BEARER_TOKENS'
check "a control plane still mounts /work (the volume claim is unchanged)" has 'mountPath: /work'
check "the probes still use /healthz for a control plane" count 'path: /healthz' 3
helm template coder "$chart" --namespace coder-ns --set config.role=control-plane \
  --set externalSecrets.properties.modelApiKey=null \
  --set externalSecrets.properties.githubToken=null > "$out"
check "a control plane renders without the two worker secret properties" lacks "$secrets_of_workers"
# The binary trims and case-folds ROLE, so the chart must too.
helm template coder "$chart" --namespace coder-ns --set-string 'config.role= Control-Plane ' \
  --set externalSecrets.properties.modelApiKey=null \
  --set externalSecrets.properties.githubToken=null > "$out"
check "a padded, mixed-case control-plane role is a control plane too" lacks "$model_and_github"

# The roles that run workers keep all of it, and their two secrets are required.
for role in "" all worker; do
  if [ -n "$role" ]; then set -- --set "config.role=$role"; else set --; fi
  label=${role:-default}
  helm template coder "$chart" --namespace coder-ns "$@" > "$out"
  check "the $label role gets the model, GitHub and OpenCode settings" count "$model_and_github" 5
  check "the $label role gets the workspace and check settings" count "$workspace_and_checks" 10
  check "the $label role gets both worker secrets, in the pod and in the ExternalSecret" count "$secrets_of_workers" 2
  check "the $label role reads both worker secrets from the Secret" count '^                  key: (MODEL_API_KEY|GITHUB_TOKEN)$' 2
  for property in modelApiKey githubToken; do
    check "the $label role fails to render without externalSecrets.properties.$property" \
      fails helm template coder "$chart" --namespace coder-ns "$@" --set "externalSecrets.properties.$property=null"
  done
done

# topology=split: a front Deployment (control plane, no volume) and the worker StatefulSet.
helm template coder "$chart" --namespace coder-ns --set topology=split > "$out"
check "split: one Deployment" count '^kind: Deployment$' 1
check "split: one StatefulSet" count '^kind: StatefulSet$' 1
check "split: exactly one Service" count '^kind: Service$' 1
check "split: still never exposed" lacks '^kind: (Ingress|IngressRoute|HTTPRoute|Gateway)$|type: (LoadBalancer|NodePort)'
check "split: the Service is still named coder (the orchestrator's URL is unchanged)" dhas Service '^  name: coder$'
check "split: the Service selects the front pods" dhas Service '^    app.kubernetes.io/name: coder-front$'
check "split: the agent card URL is still the Service" has 'http://coder.coder-ns.svc.cluster.local:8080/'
check "split: the Deployment is the front" dhas Deployment '^  name: coder-front$'
check "split: the Deployment is a control plane" dhas Deployment 'value: "control-plane"'
check "split: the Deployment has no volume" dlacks Deployment 'volumeMounts:|volumeClaimTemplates:|mountPath:'
check "split: the Deployment has no model, GitHub or OpenCode settings" dlacks Deployment "$model_and_github"
check "split: the Deployment has no workspace or check settings" dlacks Deployment "$workspace_and_checks"
check "split: the Deployment allows no local-process MCP server" dlacks Deployment 'name: MCP_ALLOW_STDIO'
check "split: the worker StatefulSet allows them" dhas StatefulSet 'name: MCP_ALLOW_STDIO'
check "split: the Deployment keeps the A2A tokens, the database and the public URL" dcount Deployment "$front" 3
check "split: the Deployment uses /healthz for all three probes" dcount Deployment 'path: /healthz' 3
check "split: the Deployment runs as uid 10001, non-root" dhas Deployment 'runAsUser: 10001'
check "split: the Deployment uses the same image" dhas Deployment 'image: "ghcr.io/vymalo/another-adam-rs/coder:'
check "split: the StatefulSet is still named coder and is the worker" dhas StatefulSet 'value: "worker"'
check "split: the worker mounts /work" dhas StatefulSet 'mountPath: /work'
check "split: the worker has neither the A2A tokens nor the public URL" dlacks StatefulSet 'name: (A2A_BEARER_TOKENS|PUBLIC_URL)$'
check "split: the worker keeps the database" dhas StatefulSet 'name: DATABASE_URL$'
check "split: the worker gets the model, GitHub and OpenCode settings" dcount StatefulSet "$model_and_github" 5
check "split: the worker gets the workspace and check settings" dcount StatefulSet "$workspace_and_checks" 10
check "split: the worker keeps one replica" dhas StatefulSet '^  replicas: 1$'
check "split: the StatefulSet identity equals the combined one (the PVC is reused)" \
  [ "$(sts_identity "$out")" = "$(sts_identity "$golden")" ]
check "split: the ExternalSecret carries all three keys" dcount ExternalSecret 'secretKey:' 3
check "split: the NetworkPolicy lists the worker's name" dhas NetworkPolicy '^          - coder$'
check "split: the NetworkPolicy lists the front's name" dhas NetworkPolicy '^          - coder-front$'
check "split: the NetworkPolicy still restricts ingress only" lacks '^    - Egress$'
check "split: no PodDisruptionBudget for one front replica" lacks '^kind: PodDisruptionBudget$'
check "split: an unset config.role is not rendered as ROLE=all" lacks 'value: "all"'

helm template coder "$chart" --namespace coder-ns --set topology=split --set front.replicas=2 > "$out"
check "split: two front replicas render a PodDisruptionBudget" has '^kind: PodDisruptionBudget$'
check "split: the PodDisruptionBudget keeps one front pod" dhas PodDisruptionBudget '^  minAvailable: 1$'
check "split: the front replicas are values-driven" dhas Deployment '^  replicas: 2$'
check "split: the worker stays at one replica" dhas StatefulSet '^  replicas: 1$'

# Guards: a bad topology, a role set by hand in split, and more than one worker stop the render.
check "topology=bogus fails to render" fails helm template coder "$chart" --namespace coder-ns --set topology=bogus
check "split with config.role=worker fails to render" \
  fails helm template coder "$chart" --namespace coder-ns --set topology=split --set config.role=worker
check "split with config.role=control-plane fails to render" \
  fails helm template coder "$chart" --namespace coder-ns --set topology=split --set config.role=control-plane
check "split with replicaCount=2 fails to render without a workspace.placement" \
  fails helm template coder "$chart" --namespace coder-ns --set topology=split --set replicaCount=2
check "combined with replicaCount=2 fails to render without a workspace.placement" \
  fails helm template coder "$chart" --namespace coder-ns --set replicaCount=2
check "split needs the worker secrets" \
  fails helm template coder "$chart" --namespace coder-ns --set topology=split --set externalSecrets.properties.githubToken=null

# workspace.placement: where /work lives and whether runs are pinned to a worker (ADR 0002).
placement_env='name: (WORKSPACE_PLACEMENT|WORKER_ID)$'
rwx_class=longhorn-rwx
says() { printf '%s\n' "$1" | grep -q -- "$2"; } # says <output> <pattern>

helm template coder "$chart" --namespace coder-ns > "$out"
check "default: no placement env (the golden is the per-pod volume)" lacks "$placement_env"
check "default: a volumeClaimTemplates volume" count '^  volumeClaimTemplates:$' 1
check "default: no PersistentVolumeClaim object" lacks '^kind: PersistentVolumeClaim$'
check "default: no pod-level volumes" lacks '^      volumes:$'

for p in isolated affinity shared; do
  for n in 1 3; do
    helm template coder "$chart" --namespace coder-ns --set workspace.placement=$p \
      --set workspace.sharedVolume.storageClass=$rwx_class --set replicaCount=$n > "$out"
    check "$p x$n: renders the replicas" has "^  replicas: $n$"
    check "$p x$n: WORKSPACE_PLACEMENT=$p" dhas StatefulSet "^              value: \"$p\"$"
    check "$p x$n: exactly one WORKSPACE_PLACEMENT" dcount StatefulSet 'name: WORKSPACE_PLACEMENT$' 1
    check "$p x$n: /work is still the workspace root and mount" dhas StatefulSet 'mountPath: /work'
    check "$p x$n: the workspace and check settings are unchanged" dcount StatefulSet "$workspace_and_checks" 10
    check "$p x$n: still never exposed" lacks '^kind: (Ingress|IngressRoute|HTTPRoute|Gateway)$|type: (LoadBalancer|NodePort)'
  done
done

# WORKER_ID: the pod name (downward API), only where runs are pinned.
for p in isolated affinity; do
  helm template coder "$chart" --namespace coder-ns --set workspace.placement=$p \
    --set workspace.sharedVolume.storageClass=$rwx_class --set replicaCount=3 > "$out"
  check "$p: WORKER_ID is set" dhas StatefulSet '^            - name: WORKER_ID$'
  check "$p: WORKER_ID reads metadata.name (the stable pod name)" dhas StatefulSet 'fieldPath: metadata.name'
  check "$p: exactly one WORKER_ID" dcount StatefulSet 'name: WORKER_ID$' 1
done
helm template coder "$chart" --namespace coder-ns --set workspace.placement=shared \
  --set workspace.sharedVolume.storageClass=$rwx_class --set replicaCount=3 > "$out"
check "shared: no WORKER_ID (runs are not pinned)" dlacks StatefulSet 'name: WORKER_ID$'

# Volumes: isolated is a claim per pod; affinity and shared are one ReadWriteMany claim.
helm template coder "$chart" --namespace coder-ns --set workspace.placement=isolated --set replicaCount=3 > "$out"
check "isolated: per-pod volumeClaimTemplates on the persistence class" dhas StatefulSet 'storageClassName: "longhorn"'
check "isolated: one claim template, ReadWriteOnce" dcount StatefulSet '^          - ReadWriteOnce$' 1
check "isolated: no shared claim object" lacks '^kind: PersistentVolumeClaim$'
check "isolated: no pod-level volumes" dlacks StatefulSet '^      volumes:$'

for p in affinity shared; do
  helm template coder "$chart" --namespace coder-ns --set workspace.placement=$p \
    --set workspace.sharedVolume.storageClass=$rwx_class --set replicaCount=3 > "$out"
  check "$p: exactly one PersistentVolumeClaim" count '^kind: PersistentVolumeClaim$' 1
  check "$p: the claim is ReadWriteMany" dhas PersistentVolumeClaim '^    - ReadWriteMany$'
  check "$p: the claim uses workspace.sharedVolume.storageClass" dhas PersistentVolumeClaim "storageClassName: \"$rwx_class\""
  check "$p: the claim is named <release>-work" dhas PersistentVolumeClaim '^  name: coder-work$'
  check "$p: the claim is kept on helm uninstall" dhas PersistentVolumeClaim 'helm.sh/resource-policy: keep'
  check "$p: the StatefulSet mounts that claim as work" dhas StatefulSet '^            claimName: coder-work$'
  check "$p: no per-pod volumeClaimTemplates" dlacks StatefulSet 'volumeClaimTemplates:'
  check "$p: the default shared claim size is 50Gi" dhas PersistentVolumeClaim 'storage: "50Gi"'
done

# An existing claim is used as it is: the chart creates none.
for p in affinity shared; do
  helm template coder "$chart" --namespace coder-ns --set workspace.placement=$p \
    --set workspace.sharedVolume.existingClaim=my-rwx --set replicaCount=2 > "$out"
  check "$p: existingClaim creates no PersistentVolumeClaim" lacks '^kind: PersistentVolumeClaim$'
  check "$p: existingClaim is the claim mounted" dhas StatefulSet '^            claimName: my-rwx$'
done
helm template coder "$chart" --namespace coder-ns --set workspace.placement=shared \
  --set workspace.sharedVolume.storageClass=$rwx_class --set workspace.sharedVolume.size=200Gi \
  --set 'workspace.sharedVolume.accessModes={ReadWriteMany,ReadWriteOnce}' > "$out"
check "shared: the claim size is values-driven" dhas PersistentVolumeClaim 'storage: "200Gi"'
check "shared: the access modes are values-driven" dcount PersistentVolumeClaim '^    - (ReadWriteMany|ReadWriteOnce)$' 2
check "isolated ignores workspace.sharedVolume (nothing to set)" \
  helm template coder "$chart" --namespace coder-ns --set workspace.placement=isolated --set replicaCount=2

# The placement is trimmed and case-folded, as the binary parses it.
helm template coder "$chart" --namespace coder-ns --set-string 'workspace.placement= Isolated ' > "$out"
check "a padded, mixed-case placement is normalised" dhas StatefulSet '^              value: "isolated"$'

# A combined pod that runs no workers reads neither variable, and needs no placement to scale.
helm template coder "$chart" --namespace coder-ns --set config.role=control-plane \
  --set workspace.placement=isolated --set replicaCount=3 \
  --set externalSecrets.properties.modelApiKey=null --set externalSecrets.properties.githubToken=null > "$out"
check "a control plane gets no placement env" lacks "$placement_env"
check "a control plane with replicaCount=3 needs no placement" \
  helm template coder "$chart" --namespace coder-ns --set config.role=control-plane --set replicaCount=3 \
    --set externalSecrets.properties.modelApiKey=null --set externalSecrets.properties.githubToken=null

# topology=split: the placement goes to the worker StatefulSet only; the front has no volume or env.
for p in isolated affinity shared; do
  helm template coder "$chart" --namespace coder-ns --set topology=split --set front.replicas=2 \
    --set workspace.placement=$p --set workspace.sharedVolume.storageClass=$rwx_class --set replicaCount=3 > "$out"
  check "split + $p: the worker has the placement" dhas StatefulSet "^              value: \"$p\"$"
  check "split + $p: the worker runs 3 replicas" dhas StatefulSet '^  replicas: 3$'
  check "split + $p: the worker is still ROLE=worker" dhas StatefulSet 'value: "worker"'
  check "split + $p: the front has no placement env, volume or claim" dlacks Deployment "$placement_env|volumes:|claimName|volumeMounts:"
  check "split + $p: the front is still a control plane" dhas Deployment 'value: "control-plane"'
done
helm template coder "$chart" --namespace coder-ns --set topology=split \
  --set workspace.placement=isolated --set replicaCount=3 > "$out"
check "split + isolated: the StatefulSet identity equals the combined one (the PVCs are reused)" \
  [ "$(sts_identity "$out")" = "$(sts_identity "$golden")" ]

# Guards.
message=$(helm template coder "$chart" --namespace coder-ns --set replicaCount=3 2>&1 || true)
check "replicaCount=3 with an empty placement fails" fails helm template coder "$chart" --namespace coder-ns --set replicaCount=3
check "the replicaCount error names workspace.placement" says "$message" 'needs workspace.placement'
check "split with replicaCount=3 and an empty placement fails" \
  fails helm template coder "$chart" --namespace coder-ns --set topology=split --set replicaCount=3
message=$(helm template coder "$chart" --namespace coder-ns --set workspace.placement=bogus 2>&1 || true)
check "an unknown placement fails" fails helm template coder "$chart" --namespace coder-ns --set workspace.placement=bogus
check "the unknown-placement error lists the accepted values and the value" \
  says "$message" 'must be one of shared, affinity or isolated.*"bogus"'
message=$(helm template coder "$chart" --namespace coder-ns --set workspace.placement=a2a-only 2>&1 || true)
check "a2a-only fails for the coder" fails helm template coder "$chart" --namespace coder-ns --set workspace.placement=a2a-only
check "the a2a-only error says the coder refuses it" says "$message" 'a2a-only is refused for the coder'
check "a2a-only fails even with one replica and a shared class" \
  fails helm template coder "$chart" --namespace coder-ns --set workspace.placement=a2a-only \
    --set workspace.sharedVolume.storageClass=$rwx_class
for p in affinity shared; do
  message=$(helm template coder "$chart" --namespace coder-ns --set workspace.placement=$p 2>&1 || true)
  check "$p without a storage class or an existing claim fails" \
    fails helm template coder "$chart" --namespace coder-ns --set workspace.placement=$p
  check "$p: the error names workspace.sharedVolume" says "$message" 'workspace.sharedVolume.storageClass'
done
check "an empty placement string is the unset default" \
  helm template coder "$chart" --namespace coder-ns --set workspace.placement=

# github.auth: a personal access token (the default) or a GitHub App installation (ADR 0009).
app_env='name: (GITHUB_APP_ID|GITHUB_APP_INSTALLATION_ID|GITHUB_APP_PRIVATE_KEY_PATH)$'
helm_app() { # the three values an App needs, then any more
  helm template coder "$chart" --namespace coder-ns --set github.auth=app --set github.app.id=1234567 \
    --set github.app.installationId=98765432 --set github.app.privateKeySecret=coder-github-app "$@"
}

helm template coder "$chart" --namespace coder-ns > "$out"
check "token (default): no GitHub App variable, volume or mount" lacks "$app_env|github-app"
check "token (default): GITHUB_TOKEN is in the pod, its Secret reference and the ExternalSecret" count 'GITHUB_TOKEN$' 3
helm template coder "$chart" --namespace coder-ns --set image.tag=sha-abc1234 --set github.auth=token > "$out"
check "github.auth=token renders the golden" cmp -s "$out" "$golden"

helm_app > "$out"
check "app: the pod gets the App's ID, installation and key path" count "$app_env" 3
check "app: GITHUB_APP_ID is values-driven" dhas StatefulSet '^              value: "1234567"$'
check "app: GITHUB_APP_INSTALLATION_ID is values-driven" dhas StatefulSet '^              value: "98765432"$'
check "app: the key is a file under /var/run/secrets/github-app" \
  dhas StatefulSet '^              value: /var/run/secrets/github-app/private-key.pem$'
check "app: no GITHUB_TOKEN, in the pod or in the ExternalSecret" lacks 'GITHUB_TOKEN'
check "app: the ExternalSecret keeps the model key and the A2A tokens" dcount ExternalSecret 'secretKey:' 2
check "app: the key comes from the named Secret, read-only" \
  dhas StatefulSet '^            secretName: coder-github-app$'
check "app: the Secret is mounted read-only, and only the key" \
  dcount StatefulSet '^              (mountPath: /var/run/secrets/github-app|readOnly: true)$' 2
check "app: the key file is group-readable only" dhas StatefulSet '^            defaultMode: 288$|^            defaultMode: 0440$'
check "app: the per-pod /work volume is untouched" dhas StatefulSet '^  volumeClaimTemplates:$'
check "app: no Secret object and no key in the render" lacks '^kind: Secret$|PRIVATE KEY'
check "app: still never exposed" lacks '^kind: (Ingress|IngressRoute|HTTPRoute|Gateway)$|type: (LoadBalancer|NodePort)'
check "app: the model, workspace and check settings are unchanged" dcount StatefulSet "$workspace_and_checks" 10
check "app: the model settings stay (the token is the only one gone)" count "$model_and_github" 4
helm_app --set externalSecrets.properties.githubToken=null > "$out"
check "app: externalSecrets.properties.githubToken is not needed" count "$app_env" 3

# A number from a values file is a float64 to Helm: it must not print as 1.234567e+06.
cat > "$vals" <<'YAML'
github:
  auth: app
  app:
    id: 1234567
    installationId: 98765432
    privateKeySecret: coder-github-app
YAML
helm template coder "$chart" --namespace coder-ns -f "$vals" > "$out"
check "app: numbers from a values file keep their digits" has '^              value: "(1234567|98765432)"$'
check "app: no number is written in exponent form" lacks 'value: "[0-9.]+e\+[0-9]+"'
helm_app --set github.app.id=Iv1.abc123 > "$out"
check "app: a client ID is used as it is" dhas StatefulSet '^              value: "Iv1.abc123"$'

# The roles: a control plane renders no GitHub setting, and needs none of the three values.
helm_app --set config.role=control-plane > "$out"
check "app: a control plane gets no App variable and no key volume" lacks "$app_env|github-app"
helm template coder "$chart" --namespace coder-ns --set github.auth=app --set config.role=control-plane > "$out"
check "app: a control plane renders without the three values" lacks "$app_env|github-app"
for role in all worker; do
  helm_app --set config.role=$role > "$out"
  check "app: the $role role gets the App's variables and the key volume" count "$app_env" 3
done

# topology=split: the worker has the key; the front has none of it.
helm_app --set topology=split > "$out"
check "split + app: the worker gets the App's variables" dcount StatefulSet "$app_env" 3
check "split + app: the worker mounts the key Secret" dhas StatefulSet '^            secretName: coder-github-app$'
check "split + app: the front has no App variable, volume or mount" dlacks Deployment "$app_env|github-app|volumes:|volumeMounts:"
check "split + app: the StatefulSet identity equals the combined one (the PVC is reused)" \
  [ "$(sts_identity "$out")" = "$(sts_identity "$golden")" ]
check "split + app without the three values fails (the worker needs them)" \
  fails helm template coder "$chart" --namespace coder-ns --set topology=split --set github.auth=app

# A shared volume and the key are two volumes of one pod.
helm_app --set workspace.placement=shared --set workspace.sharedVolume.storageClass=$rwx_class --set replicaCount=3 > "$out"
check "shared + app: the pod mounts the shared claim and the key Secret" \
  dcount StatefulSet '^        - name: (work|github-app)$' 2
check "shared + app: the claim is still coder-work" dhas StatefulSet '^            claimName: coder-work$'
check "shared + app: still no per-pod volumeClaimTemplates" dlacks StatefulSet 'volumeClaimTemplates:'

# Guards.
message=$(helm template coder "$chart" --namespace coder-ns --set github.auth=bogus 2>&1 || true)
check "an unknown github.auth fails" fails helm template coder "$chart" --namespace coder-ns --set github.auth=bogus
check "the unknown-github.auth error says token or app and shows the value" says "$message" 'must be token or app.*"bogus"'
for missing in id installationId privateKeySecret; do
  message=$(helm_app --set "github.app.$missing=" 2>&1 || true)
  check "app without github.app.$missing fails" fails helm_app --set "github.app.$missing="
  check "app without github.app.$missing: the error names it" says "$message" "github.app.$missing"
done
for bad in 0 -5 abc 12x 1.5; do
  check "app with installationId=$bad fails" fails helm_app --set-string "github.app.installationId=$bad"
done
check "app with a numeric installationId from --set renders" helm_app --set github.app.installationId=42
check "token mode ignores the github.app values" \
  helm template coder "$chart" --namespace coder-ns --set github.app.id= --set github.app.installationId=nope

# The GitHub MCP server: a native sidecar (an init container with restartPolicy Always) of every pod that
# runs workers, in http mode with no credential at all (ADR 0017, D4). The coder sends the token of each call.
# sidecar [file]: the init containers of the StatefulSet (just the sidecar), from the render or a file.
sidecar() {
  doc StatefulSet "${1:-$out}" | awk '/^      initContainers:$/ { on = 1; next } /^      containers:$/ { on = 0 } on'
}
coder_container() { # the coder's own container of the StatefulSet
  doc StatefulSet "${1:-$out}" | awk '/^      containers:$/ { on = 1 } /^      (volumes|nodeSelector|affinity|tolerations):$/ { on = 0 } on'
}
shas() { sidecar "$out" | grep -Eq -- "$1"; }
slacks() { ! sidecar "$out" | grep -Eq -- "$1"; }
chas() { coder_container "$out" | grep -Eq -- "$1"; }
clacks() { ! coder_container "$out" | grep -Eq -- "$1"; }

helm template coder "$chart" --namespace coder-ns > "$out"
check "the pod has one init container, the GitHub MCP server" count '^        - name: github-mcp$' 1
check "it is a native sidecar: restartPolicy Always" shas '^          restartPolicy: Always$'
check "it is the coder's own image" shas '^          image: "ghcr.io/vymalo/another-adam-rs/coder:sha-[0-9a-zA-Z]+"$'
check "it runs github-mcp-server in http mode" shas '^          command: \["tini", "--", "github-mcp-server"\]$'
check "it is read-only" shas '^            - --read-only$'
check "it has the four toolsets the coder's tools allow-list assumes" shas '^            - context,repos,issues,pull_requests$'
check "it listens on loopback only: --listen-host 127.0.0.1" shas '^            - 127.0.0.1$'
check "it listens on 8082" shas '^            - "8082"$'
check "it is probed on its port before the coder starts: a startupProbe" shas 'startupProbe:'
check "the probe connects to 8082 on loopback from inside the container (the server listens there only)" \
  shas '^              command: \["bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/8082"\]$'
check "the probe is not a kubelet TCP probe: that one connects to the pod IP, which the server does not listen on" slacks 'tcpSocket:'
check "it holds no credential: no env, Secret, volume, token or key" \
  slacks 'env:|secretKeyRef|secretName|volumeMounts|GITHUB_TOKEN|GITHUB_APP|MODEL_API_KEY|PRIVATE KEY|name: GITHUB_HOST'
check "it has the container security context" shas 'allowPrivilegeEscalation: false'
check "it has resources" shas '^          resources:$'
check "the coder is told where it is: GITHUB_MCP_URL, once" count 'name: GITHUB_MCP_URL$' 1
check "GITHUB_MCP_URL is on loopback at the port" chas '^              value: "http://127.0.0.1:8082"$'
check "the coder still has the GitHub settings it had" count "$model_and_github" 5
check "the sidecar adds no health probe path (the coder's three are the only httpGet ones)" count 'path: /healthz' 3

# Values-driven: the port (and the coder's URL with it), the host (only the sidecar has it), off.
helm template coder "$chart" --namespace coder-ns --set githubMcp.port=9100 --set githubMcp.host=ghe.example.com > "$out"
check "the port is values-driven in the arguments" shas '^            - "9100"$'
check "the port is values-driven in the probe" shas '/dev/tcp/127.0.0.1/9100"\]$'
check "GITHUB_MCP_URL follows the port" chas '^              value: "http://127.0.0.1:9100"$'
check "the host is the sidecar's GITHUB_HOST" shas 'name: GITHUB_HOST'
check "GITHUB_HOST carries the host" shas '^              value: "ghe.example.com"$'
check "the coder's container has no GITHUB_HOST" clacks 'GITHUB_HOST'
helm template coder "$chart" --namespace coder-ns --set githubMcp.enabled=false > "$out"
check "githubMcp.enabled=false renders no sidecar and no GITHUB_MCP_URL" lacks 'initContainers:|name: github-mcp$|GITHUB_MCP_URL|restartPolicy: Always'
check "an invalid githubMcp.port fails to render" \
  fails helm template coder "$chart" --namespace coder-ns --set githubMcp.port=70000
check "port 0 fails to render" fails helm template coder "$chart" --namespace coder-ns --set githubMcp.port=0
check "a port that is no number fails to render" \
  fails helm template coder "$chart" --namespace coder-ns --set-string githubMcp.port=http

# Only the roles that run workers connect an MCP server: a control plane, and the split front, have none.
helm template coder "$chart" --namespace coder-ns --set config.role=control-plane > "$out"
check "a control plane renders no sidecar and no GITHUB_MCP_URL" lacks 'initContainers:|name: github-mcp$|GITHUB_MCP_URL'
for role in all worker; do
  helm template coder "$chart" --namespace coder-ns --set config.role=$role > "$out"
  check "the $role role runs the sidecar" count '^        - name: github-mcp$' 1
done
helm template coder "$chart" --namespace coder-ns --set topology=split > "$out"
check "split: the worker StatefulSet runs the sidecar" dhas StatefulSet '^        - name: github-mcp$'
check "split: the worker is told where it is" dhas StatefulSet 'name: GITHUB_MCP_URL$'
check "split: the front Deployment has no sidecar and no GITHUB_MCP_URL" dlacks Deployment 'initContainers:|github-mcp|GITHUB_MCP_URL'
check "split: the StatefulSet identity still equals the combined one (the PVC is reused)" \
  [ "$(sts_identity "$out")" = "$(sts_identity "$golden")" ]
helm_app > "$out"
check "app: the sidecar still holds no key and mounts nothing" \
  slacks 'github-app|secretName|volumeMounts|GITHUB_APP|PRIVATE KEY'
check "app: the key is mounted into the coder's container only" chas 'mountPath: /var/run/secrets/github-app'
helm template coder "$chart" --namespace coder-ns --set workspace.placement=shared \
  --set workspace.sharedVolume.storageClass=$rwx_class --set replicaCount=3 > "$out"
check "shared: every worker has the sidecar (it is part of the pod)" count '^        - name: github-mcp$' 1

# github.app.owners: no pin, the installation of each owner is found (ADR 0017). Exactly one of the pin and the
# owners; the owners are what the coder may act for, and there is no default.
owners_env='name: (GITHUB_APP_ID|GITHUB_APP_OWNERS|GITHUB_APP_PRIVATE_KEY_PATH)$'
helm_owners() { # the values an App with owners needs, then any more
  helm template coder "$chart" --namespace coder-ns --set github.auth=app --set github.app.id=1234567 \
    --set 'github.app.owners={acme,Other-Org}' --set github.app.privateKeySecret=coder-github-app "$@"
}
helm_owners > "$out"
check "owners: the pod gets the App's ID, the owners and the key path" count "$owners_env" 3
check "owners: no GITHUB_APP_INSTALLATION_ID, there is no pin" lacks 'GITHUB_APP_INSTALLATION_ID'
check "owners: GITHUB_APP_OWNERS is the list, comma-joined, as written" dhas StatefulSet '^              value: "acme,Other-Org"$'
check "owners: exactly one GITHUB_APP_OWNERS" count 'name: GITHUB_APP_OWNERS$' 1
check "owners: no GITHUB_TOKEN, the key is a file from the named Secret, read-only" \
  dhas StatefulSet '^            secretName: coder-github-app$'
check "owners: the key is mounted into the coder's container only (not the sidecar)" chas 'mountPath: /var/run/secrets/github-app'
check "owners: the GitHub MCP sidecar still holds nothing" slacks 'github-app|secretName|GITHUB_APP|volumeMounts'
check "owners: no Secret object and no key in the render" lacks '^kind: Secret$|PRIVATE KEY'
check "owners: the model, workspace and check settings are unchanged" dcount StatefulSet "$workspace_and_checks" 10
check "owners: still never exposed" lacks '^kind: (Ingress|IngressRoute|HTTPRoute|Gateway)$|type: (LoadBalancer|NodePort)'
# A pin renders as before, and the owners value is empty by default.
helm_app > "$out"
check "a pin: GITHUB_APP_INSTALLATION_ID and no GITHUB_APP_OWNERS" lacks 'GITHUB_APP_OWNERS'
check "a pin: it is rendered once" count 'name: GITHUB_APP_INSTALLATION_ID$' 1
# The forms the owners come in.
cat > "$vals" <<'YAML'
github:
  auth: app
  app:
    id: 1234567
    privateKeySecret: coder-github-app
    owners: "acme, Other-Org  third,"
YAML
helm template coder "$chart" --namespace coder-ns -f "$vals" > "$out"
check "owners: a string of names separated by commas or spaces is accepted" dhas StatefulSet '^              value: "acme,Other-Org,third"$'
cat > "$vals" <<'YAML'
github:
  auth: app
  app:
    id: 1234567
    privateKeySecret: coder-github-app
    owners: ["  acme ", "", 42]
YAML
helm template coder "$chart" --namespace coder-ns -f "$vals" > "$out"
check "owners: entries are trimmed, blanks dropped, a number is its digits" dhas StatefulSet '^              value: "acme,42"$'
helm_owners --set 'github.app.owners={*}' > "$out"
check "owners: * is passed through (the binary warns, and refuses it beside names)" dhas StatefulSet '^              value: "\*"$'
# Roles: a control plane renders none of it, the split worker has it, the front has not.
helm_owners --set config.role=control-plane > "$out"
check "owners: a control plane gets no App variable and no key volume" lacks "$owners_env|github-app"
helm_owners --set topology=split > "$out"
check "split + owners: the worker gets the owners" dhas StatefulSet 'name: GITHUB_APP_OWNERS$'
check "split + owners: the front has no App variable, volume or mount" dlacks Deployment "$owners_env|github-app|volumes:|volumeMounts:"
# Guards.
message=$(helm_owners --set github.app.installationId=98765432 2>&1 || true)
check "a pin and owners together fail" fails helm_owners --set github.app.installationId=98765432
check "the error says both are set and names both values" says "$message" 'github.app.installationId and github.app.owners are both set'
message=$(helm_app --set 'github.app.installationId=' 2>&1 || true)
check "neither the pin nor owners fails" fails helm_app --set github.app.installationId=
check "the error names both ways" says "$message" 'github.app.installationId.*github.app.owners'
check "an empty owners list with no pin fails" fails helm_owners --set 'github.app.owners=null'
cat > "$vals" <<'YAML'
github:
  auth: app
  app:
    id: 1234567
    privateKeySecret: coder-github-app
    owners: [" ", ""]
YAML
check "a blank-only owners list with no pin fails" fails helm template coder "$chart" --namespace coder-ns -f "$vals"
check "a bad pin is not mistaken for no pin: installationId=abc with owners fails" \
  fails helm_owners --set-string github.app.installationId=abc
check "token mode ignores github.app.owners" \
  helm template coder "$chart" --namespace coder-ns --set 'github.app.owners={acme}'
helm template coder "$chart" --namespace coder-ns --set 'github.app.owners={acme}' > "$out"
check "token mode renders no GITHUB_APP_OWNERS" lacks 'GITHUB_APP_OWNERS'

# The extra MCP servers (values `mcp.*`): our web search server and Context7, off by default. With either
# on, the chart renders ONE file of servers (a ConfigMap in the shape of mcp.json) and sets ADAM_EXTRA_MCP_FILE
# on the workers; the binary merges it over the agent's own mcp.json, so the chart carries no copy of the agent's
# files. The keys come from the ExternalSecret as env vars that the file names as ${VAR}; never as chart values.
helm template coder "$chart" --namespace coder-ns > "$out"
check "off: no ConfigMap, no ADAM_EXTRA_MCP_FILE, no extra key, no MCP_ALLOW_INSECURE, no checksum" \
  lacks 'kind: ConfigMap|ADAM_EXTRA_MCP_FILE|extra-mcp|SEARCH_MCP_TOKEN|CONTEXT7_API_KEY|MCP_ALLOW_INSECURE|checksum/'
check "the chart ships no copy of the agent's files" [ ! -e "$chart/agent" ]
helm template coder "$chart" --namespace coder-ns --set image.tag=sha-abc1234 --set mcp.websearch.url= --set mcp.context7.enabled=false > "$out"
check "off, set explicitly: the render equals the golden" cmp -s "$out" "$golden"
helm template coder "$chart" --namespace coder-ns --set image.tag=sha-abc1234 --set-string mcp.context7.enabled=false > "$out"
check "the string \"false\" is off, not on" cmp -s "$out" "$golden"

mcp_json() { doc ConfigMap "${1:-$out}" | awk '/^  mcp.json: \|$/ { on = 1; next } on { sub(/^    /, ""); print }'; }
# jqt <filter> [jq args]: true when the filter holds on the rendered mcp.json.
jqt() { filter=$1; shift; mcp_json "$out" | jq -e "$@" "$filter" >/dev/null; }
search_url=http://search-mcp.coder-ns.svc.cluster.local:8080/mcp
helm_mcp() { helm template coder "$chart" --namespace coder-ns "$@"; }

helm_mcp --set mcp.websearch.url=$search_url --set mcp.websearch.allowInsecure=true > "$out"
check "websearch: one ConfigMap, the servers file" count '^kind: ConfigMap$' 1
check "websearch: it parses as JSON" jqt '.'
check "websearch: only mcpServers at the top" jqt 'keys == ["mcpServers"]'
check "websearch: only websearch (no github: that is the agent's own, nothing is copied)" jqt '.mcpServers | keys == ["websearch"]'
check "websearch: type http, the URL as set" jqt '.mcpServers.websearch | .type == "http" and .url == $u' --arg u "$search_url"
check "websearch: the header is Authorization: Bearer \${SEARCH_MCP_TOKEN}" \
  jqt '.mcpServers.websearch.headers == {"Authorization": "Bearer ${SEARCH_MCP_TOKEN}"}'
check "websearch: optional by default, no tools allow-list" jqt '.mcpServers.websearch | .optional == true and (has("tools") | not)'
check "websearch: every server entry has only the keys mcp.json knows" \
  jqt '[.mcpServers[] | keys[] ] | all(. == "type" or . == "url" or . == "headers" or . == "tools" or . == "optional")'
check "websearch: server ids are valid (letters, digits, - and _, no __)" \
  jqt '.mcpServers | keys | all(test("^[A-Za-z0-9_-]{1,64}$") and (contains("__") | not))'
check "websearch: the ExternalSecret copies SEARCH_MCP_TOKEN" dhas ExternalSecret 'secretKey: SEARCH_MCP_TOKEN$'
check "websearch: ... from the property search_mcp_token" dhas ExternalSecret 'property: search_mcp_token$'
check "websearch: ... and nothing for Context7" dlacks ExternalSecret 'CONTEXT7'
check "websearch: the worker reads SEARCH_MCP_TOKEN as a secretKeyRef, never a value" \
  [ "$(doc StatefulSet | grep -A1 'name: SEARCH_MCP_TOKEN$' | tail -1 | tr -d ' ')" = "valueFrom:" ]
check "websearch: ADAM_EXTRA_MCP_FILE is the mounted file" dhas StatefulSet '^              value: "/etc/adam/extra-mcp/mcp.json"$'
check "websearch: ... the ConfigMap is mounted read-only there" dhas StatefulSet 'mountPath: /etc/adam/extra-mcp$'
check "websearch: ... as a volume of the ConfigMap" dhas StatefulSet 'name: coder-mcp$'
check "websearch: ADAM_AGENT_DIR is not set: the agent's files stay in the binary" lacks 'ADAM_AGENT_DIR'
check "websearch: the pod restarts when the file changes: checksum/extra-mcp" dhas StatefulSet 'checksum/extra-mcp: '
check "websearch: allowInsecure=true sets MCP_ALLOW_INSECURE, once" count 'name: MCP_ALLOW_INSECURE$' 1
check "websearch: no token value anywhere in the render" lacks 'Bearer [A-Za-z0-9]'
check "websearch: the NetworkPolicy still restricts ingress only (egress to the Service stays open)" \
  dlacks NetworkPolicy '^    - Egress$|^  egress:'
sum_on=$(grep 'checksum/extra-mcp' "$out")
helm_mcp --set mcp.websearch.url=${search_url}2 --set mcp.websearch.allowInsecure=true > "$out"
check "websearch: a new URL changes the checksum" [ "$sum_on" != "$(grep 'checksum/extra-mcp' "$out")" ]

helm_mcp --set mcp.context7.enabled=true > "$out"
check "context7: the servers file has only context7" jqt '.mcpServers | keys == ["context7"]'
check "context7: the verified endpoint over https, Authorization: Bearer \${CONTEXT7_API_KEY}, optional" \
  jqt '.mcpServers.context7 == {"type":"http","url":"https://mcp.context7.com/mcp","headers":{"Authorization":"Bearer ${CONTEXT7_API_KEY}"},"optional":true}'
check "context7: the ExternalSecret copies CONTEXT7_API_KEY" dhas ExternalSecret 'secretKey: CONTEXT7_API_KEY$'
check "context7: ... from the property context7_api_key" dhas ExternalSecret 'property: context7_api_key$'
check "context7: nothing for websearch, no MCP_ALLOW_INSECURE" lacks 'SEARCH_MCP_TOKEN|MCP_ALLOW_INSECURE'
check "context7: the worker reads CONTEXT7_API_KEY from the Secret" dhas StatefulSet 'name: CONTEXT7_API_KEY$'
helm_mcp --set-string mcp.context7.enabled=true > "$out"
check "context7: the string \"true\" is on" jqt '.mcpServers | keys == ["context7"]'

helm_mcp --set mcp.websearch.url=$search_url --set mcp.websearch.allowInsecure=true --set mcp.context7.enabled=true > "$out"
check "both: the servers are context7 and websearch" jqt '.mcpServers | keys == ["context7","websearch"]'
check "both: every \${VAR} of the file is an env var of the worker, from the Secret" \
  [ "$(mcp_json "$out" | grep -o '\${[A-Z0-9_]*}' | sort -u | tr -d '${}' | tr '\n' ' ')" = "CONTEXT7_API_KEY SEARCH_MCP_TOKEN " ]
check "both: the ExternalSecret has both keys beside the existing ones" \
  [ "$(doc ExternalSecret | grep -c 'secretKey:')" -eq 5 ]

# A deployment's own values: header name, prefix, tools, https, optional, other property names and AWS secret.
helm_mcp --set mcp.websearch.url=https://search.example.com/mcp --set mcp.websearch.header=X-Search-Token \
  --set mcp.websearch.valuePrefix= --set 'mcp.websearch.tools={web_search}' --set mcp.websearch.optional=false \
  --set 'mcp.context7.enabled=true' --set 'mcp.context7.tools={resolve-library-id,query-docs}' \
  --set externalSecrets.properties.searchMcpToken=other_prop --set externalSecrets.key=prod/another-agentic/env > "$out"
check "values: the header name and an empty prefix" jqt '.mcpServers.websearch.headers == {"X-Search-Token": "${SEARCH_MCP_TOKEN}"}'
check "values: tools allow-lists" \
  jqt '.mcpServers.websearch.tools == ["web_search"] and .mcpServers.context7.tools == ["resolve-library-id","query-docs"]'
check "values: optional=false leaves the key out (required is the binary's default)" \
  jqt '(.mcpServers.websearch | has("optional") | not) and .mcpServers.context7.optional == true'
check "values: an https URL needs no MCP_ALLOW_INSECURE" lacks 'MCP_ALLOW_INSECURE'
check "values: the AWS property is the deployment's" dhas ExternalSecret 'property: other_prop$'
check "values: ... and so is the AWS secret" dhas ExternalSecret 'key: prod/another-agentic/env$'
for url in http://localhost:9000/mcp http://127.0.0.1:9000/mcp http://search.localhost/mcp; do
  helm_mcp --set mcp.websearch.url=$url > "$out"
  check "a loopback http URL ($url) needs no opt-in and sets no MCP_ALLOW_INSECURE" lacks 'MCP_ALLOW_INSECURE'
done

# MCP_ALLOW_INSECURE is an explicit opt-in, never automatic: one switch for every server and for the
# thread-tools endpoints that senders announce.
check "a plain http URL to another machine fails without the opt-in" fails helm_mcp --set mcp.websearch.url=$search_url
message=$(helm_mcp --set mcp.websearch.url=$search_url 2>&1 || true)
check "... and the error names the value and says what the switch covers" \
  says "$message" 'mcp.websearch.allowInsecure.*every MCP server'
check "the deployment's own config.extraEnv MCP_ALLOW_INSECURE is the opt-in too, and is not duplicated" \
  helm_mcp --set mcp.websearch.url=$search_url --set-string config.extraEnv.MCP_ALLOW_INSECURE=true
helm_mcp --set mcp.websearch.url=$search_url --set-string config.extraEnv.MCP_ALLOW_INSECURE=true --set mcp.websearch.allowInsecure=true > "$out"
check "... one MCP_ALLOW_INSECURE, the deployment's" count 'name: MCP_ALLOW_INSECURE$' 1
check "a plain http Context7 URL needs the opt-in too" \
  fails helm_mcp --set mcp.context7.enabled=true --set mcp.context7.url=http://ctx.example.com/mcp
check "a plain http Context7 URL with the opt-in sets MCP_ALLOW_INSECURE although websearch is off" \
  helm_mcp --set mcp.context7.enabled=true --set mcp.context7.url=http://ctx.example.com/mcp --set mcp.websearch.allowInsecure=true
helm_mcp --set mcp.context7.enabled=true --set mcp.context7.url=http://ctx.example.com/mcp --set mcp.websearch.allowInsecure=true > "$out"
check "... once, on the worker" count 'name: MCP_ALLOW_INSECURE$' 1
check "extraEnv MCP_ALLOW_INSECURE=\"false\" with a plain http URL fails" \
  fails helm_mcp --set mcp.websearch.url=$search_url --set mcp.websearch.allowInsecure=true --set-string config.extraEnv.MCP_ALLOW_INSECURE=false
message=$(helm_mcp --set mcp.websearch.url=$search_url --set-string config.extraEnv.MCP_ALLOW_INSECURE=false 2>&1 || true)
check "... and the error says so" says "$message" 'config.extraEnv.MCP_ALLOW_INSECURE is "false"'
check "extraEnv MCP_ALLOW_INSECURE=\"false\" with an https URL is the deployment's business" \
  helm_mcp --set mcp.websearch.url=https://search.example.com/mcp --set-string config.extraEnv.MCP_ALLOW_INSECURE=false
check "extraEnv MCP_ALLOW_INSECURE=1 counts as the opt-in" \
  helm_mcp --set mcp.websearch.url=$search_url --set-string config.extraEnv.MCP_ALLOW_INSECURE=1
check "allowInsecure with an https URL sets nothing" \
  helm_mcp --set mcp.websearch.url=https://search.example.com/mcp --set mcp.websearch.allowInsecure=true
helm_mcp --set mcp.websearch.url=https://search.example.com/mcp --set mcp.websearch.allowInsecure=true > "$out"
check "... no MCP_ALLOW_INSECURE" lacks 'MCP_ALLOW_INSECURE'
check "allowInsecure with no server on renders nothing" \
  helm_mcp --set mcp.websearch.allowInsecure=true
helm_mcp --set mcp.websearch.allowInsecure=true > "$out"
check "... and sets no MCP_ALLOW_INSECURE" lacks 'MCP_ALLOW_INSECURE'

# Roles: only a worker connects a server, so only a worker gets the file and the keys.
helm_mcp --set mcp.websearch.url=$search_url --set mcp.websearch.allowInsecure=true --set mcp.context7.enabled=true --set topology=split > "$out"
check "split: one ConfigMap, for the worker" count '^kind: ConfigMap$' 1
check "split: the worker mounts the file and is told where it is" \
  dhas StatefulSet 'mountPath: /etc/adam/extra-mcp$'
check "split: ... ADAM_EXTRA_MCP_FILE" dhas StatefulSet 'name: ADAM_EXTRA_MCP_FILE$'
check "split: ... and both keys" dhas StatefulSet 'name: SEARCH_MCP_TOKEN$'
check "split: ... and CONTEXT7_API_KEY" dhas StatefulSet 'name: CONTEXT7_API_KEY$'
check "split: the front has no file, no key, no MCP_ALLOW_INSECURE" \
  dlacks Deployment 'extra-mcp|EXTRA_MCP|SEARCH_MCP_TOKEN|CONTEXT7_API_KEY|MCP_ALLOW_INSECURE|checksum/'
helm_mcp --set mcp.websearch.url=$search_url --set mcp.websearch.allowInsecure=true --set mcp.context7.enabled=true --set config.role=control-plane > "$out"
check "control plane: no ConfigMap, no file, no key, no MCP_ALLOW_INSECURE" \
  lacks 'kind: ConfigMap|extra-mcp|EXTRA_MCP|SEARCH_MCP_TOKEN|CONTEXT7_API_KEY|MCP_ALLOW_INSECURE'
check "control plane: the ExternalSecret copies no MCP key" dlacks ExternalSecret 'SEARCH_MCP_TOKEN|CONTEXT7_API_KEY|search_mcp_token|context7_api_key'
helm_mcp --set mcp.websearch.url=$search_url --set mcp.websearch.allowInsecure=true --set github.auth=app --set github.app.id=1 --set github.app.installationId=2 \
  --set github.app.privateKeySecret=k --set workspace.placement=shared --set workspace.sharedVolume.storageClass=x --set replicaCount=2 > "$out"
check "with the App key and a shared volume the file is a third volume" count '^        - name: (extra-mcp|github-app|work)$' 3
helm_mcp --set mcp.websearch.url=$search_url --set mcp.websearch.allowInsecure=true --set githubMcp.port=9100 > "$out"
check "the GitHub sidecar and GITHUB_MCP_URL still follow githubMcp.port, the file never names github" \
  chas '^              value: "http://127.0.0.1:9100"$'
check "... the servers file has no github entry" jqt '.mcpServers | has("github") | not'

# Guards.
for bad in 'mcp.websearch.url=ftp://x/mcp' 'mcp.websearch.url=search.svc/mcp' 'mcp.websearch.url=http://user:pw@search.svc/mcp' \
           'mcp.websearch.url=http://search.svc/mcp?key=$SECRET' 'mcp.websearch.url=http://${HOST}/mcp' 'mcp.websearch.header=bad header' \
           'mcp.websearch.valuePrefix=${TOKEN}' 'mcp.websearch.header=' 'mcp.websearch.tools=web_search' 'mcp.websearch.optional=yes' \
           'mcp.websearch.allowInsecure=maybe' 'mcp.context7.enabled=perhaps'; do
  check "a bad value fails to render: $bad" fails helm_mcp --set mcp.websearch.url=https://search.svc/mcp --set-string "$bad"
done
check "tools as a scalar fails (the list is a list)" fails helm_mcp --set mcp.context7.enabled=true --set-string mcp.context7.tools=resolve-library-id
check "tools as a list is fine" helm_mcp --set mcp.context7.enabled=true --set 'mcp.context7.tools={resolve-library-id}'
check "context7 with a bad URL fails to render" fails helm_mcp --set mcp.context7.enabled=true --set mcp.context7.url=ftp://x
check "a server on with externalSecrets.enabled=false fails (a key is never a chart value)" \
  fails helm_mcp --set mcp.context7.enabled=true --set externalSecrets.enabled=false
check "a server on with its property name empty fails" fails helm_mcp --set mcp.context7.enabled=true --set externalSecrets.properties.context7ApiKey=
check "a server on with config.extraEnv.ADAM_EXTRA_MCP_FILE fails (the chart sets it)" \
  fails helm_mcp --set mcp.context7.enabled=true --set-string config.extraEnv.ADAM_EXTRA_MCP_FILE=/x
check "ADAM_AGENT_DIR in extraEnv is the deployment's own business again (unchanged)" \
  helm_mcp --set mcp.context7.enabled=true --set-string config.extraEnv.ADAM_AGENT_DIR=/x

# config.modelBaseUrlFromSecret: the model gateway's URL (MODEL_BASE_URL) from the ExternalSecret instead of
# config.modelBaseUrl, so a deployment keeps it out of git (the owner's decision of 2026-10-04). Off by default,
# byte-for-byte invisible when off (the golden above). Like MODEL_API_KEY it belongs to the roles that run workers.
placeholder='https://gateway.example.invalid/v1'
# url_env: the MODEL_BASE_URL entry of the worker StatefulSet's env, with the lines that say where its value is.
url_env() { doc StatefulSet "$out" | grep -A4 -- 'name: MODEL_BASE_URL$'; }
urlhas() { url_env | grep -Eq -- "$1"; }
urllacks() { ! url_env | grep -Eq -- "$1"; }
helm_url() { helm template coder "$chart" --namespace coder-ns "$@"; }

helm_url > "$out"
check "url off: MODEL_BASE_URL is the literal config.modelBaseUrl" urlhas "^              value: \"$placeholder\"$"
check "url off: no secretKeyRef for MODEL_BASE_URL" urllacks 'secretKeyRef|valueFrom'
check "url off: the ExternalSecret copies no MODEL_BASE_URL" dlacks ExternalSecret 'MODEL_BASE_URL|model_base_url'
helm_url --set config.modelBaseUrl=https://gw.example.com/v1 > "$out"
check "url off: config.modelBaseUrl is values-driven" urlhas '^              value: "https://gw.example.com/v1"$'
helm_url --set config.modelBaseUrlFromSecret=false --set externalSecrets.properties.modelBaseUrl=null > "$out"
check "url off: the property name is not read (null is fine)" dlacks ExternalSecret 'model_base_url|MODEL_BASE_URL'

for role in "" all worker; do
  if [ -n "$role" ]; then set -- --set "config.role=$role"; else set --; fi
  label=${role:-default}
  helm_url --set config.modelBaseUrlFromSecret=true "$@" > "$out"
  check "url on, $label role: MODEL_BASE_URL is read from the Secret" urlhas '^                  key: MODEL_BASE_URL$'
  check "url on, $label role: from the chart's Secret (named like MODEL_API_KEY's)" \
    [ "$(url_env | grep -Ec '^ +name: coder$')" -eq 1 ]
  check "url on, $label role: no literal value for MODEL_BASE_URL" urllacks '^              value:'
  check "url on, $label role: the placeholder is rendered nowhere" lacks 'gateway.example.invalid'
  check "url on, $label role: MODEL_BASE_URL is set exactly once" count 'name: MODEL_BASE_URL$' 1
  check "url on, $label role: the ExternalSecret copies the property into MODEL_BASE_URL" \
    dhas ExternalSecret '^    - secretKey: MODEL_BASE_URL$'
  check "url on, $label role: the default AWS property is model_base_url" dhas ExternalSecret '^        property: model_base_url$'
  check "url on, $label role: model, GitHub and OpenCode settings unchanged in number" count "$model_and_github" 5
  check "url on, $label role: the ExternalSecret has one more key (MODEL_API_KEY, MODEL_BASE_URL, GITHUB_TOKEN, A2A_BEARER_TOKENS)" \
    dcount ExternalSecret 'secretKey:' 4
done

helm_url --set config.modelBaseUrlFromSecret=true --set externalSecrets.properties.modelBaseUrl=gateway_url --set externalSecrets.key=prod/another-agentic/env > "$out"
check "url on: the AWS property is values-driven" dhas ExternalSecret '^        property: gateway_url$'
check "url on: it is read under externalSecrets.key" dhas ExternalSecret '^        key: prod/another-agentic/env$'
check "url on: no model_base_url when the property is renamed" dlacks ExternalSecret 'property: model_base_url'

# What counts as unset: the placeholder (the default) and an empty value, not a URL.
check "url on: the default placeholder counts as unset" helm_url --set config.modelBaseUrlFromSecret=true
check "url on: an explicit placeholder counts as unset" helm_url --set config.modelBaseUrlFromSecret=true --set "config.modelBaseUrl=$placeholder"
check "url on: an empty config.modelBaseUrl counts as unset" helm_url --set config.modelBaseUrlFromSecret=true --set-string config.modelBaseUrl=
check "url on: a null config.modelBaseUrl counts as unset" helm_url --set config.modelBaseUrlFromSecret=true --set config.modelBaseUrl=null
helm_url --set config.modelBaseUrlFromSecret=true --set-string config.modelBaseUrl= > "$out"
check "url on with an empty config.modelBaseUrl: still from the Secret, nothing literal" urlhas '^                  key: MODEL_BASE_URL$'

# The URL itself is never in the render, whatever the option (a secret property is read at runtime).
helm_url --set config.modelBaseUrlFromSecret=true --set topology=split --set front.replicas=2 > "$out"
check "url on, split: the worker reads MODEL_BASE_URL from the Secret" urlhas '^                  key: MODEL_BASE_URL$'
check "url on, split: the front has no MODEL_BASE_URL" dlacks Deployment 'MODEL_BASE_URL|model_base_url'
check "url on, split: the ExternalSecret copies it for the worker" dhas ExternalSecret '^    - secretKey: MODEL_BASE_URL$'
check "url on, split: the worker still has the five model and GitHub settings" dcount StatefulSet "$model_and_github" 5

# A control plane never reads the model: nothing is rendered, and nothing is required, even with the option on.
helm_url --set config.modelBaseUrlFromSecret=true --set config.role=control-plane \
  --set externalSecrets.properties.modelApiKey=null --set externalSecrets.properties.githubToken=null \
  --set externalSecrets.properties.modelBaseUrl=null > "$out"
check "url on, control plane: no MODEL_BASE_URL in the pod" lacks 'MODEL_BASE_URL'
check "url on, control plane: none in the ExternalSecret, no property read" lacks 'model_base_url|secretKey: MODEL_BASE_URL'
check "url on, control plane: a control plane with the option and a real URL renders (it reads neither)" \
  helm_url --set config.modelBaseUrlFromSecret=true --set config.role=control-plane --set config.modelBaseUrl=https://gw.example.com/v1 \
    --set externalSecrets.properties.modelApiKey=null --set externalSecrets.properties.githubToken=null

# Guards.
message=$(helm_url --set config.modelBaseUrlFromSecret=true --set config.modelBaseUrl=https://gw.example.com/v1 2>&1 || true)
check "url on with a real config.modelBaseUrl fails (a URL in values is written in git and ignored)" \
  fails helm_url --set config.modelBaseUrlFromSecret=true --set config.modelBaseUrl=https://gw.example.com/v1
check "... the error names both values and says what counts as unset" says "$message" 'config.modelBaseUrl.*placeholder'
check "... and never prints the URL" fails says "$message" 'gw.example.com'
check "url on with a whitespace-padded real URL fails" \
  fails helm_url --set config.modelBaseUrlFromSecret=true --set-string 'config.modelBaseUrl= https://gw.example.com/v1 '
check "url on with a real config.modelBaseUrl fails in split too" \
  fails helm_url --set topology=split --set config.modelBaseUrlFromSecret=true --set config.modelBaseUrl=https://gw.example.com/v1
message=$(helm_url --set config.modelBaseUrlFromSecret=true --set externalSecrets.properties.modelBaseUrl=null 2>&1 || true)
check "url on with no AWS property name fails" \
  fails helm_url --set config.modelBaseUrlFromSecret=true --set externalSecrets.properties.modelBaseUrl=null
check "... the error names externalSecrets.properties.modelBaseUrl" says "$message" 'externalSecrets.properties.modelBaseUrl'
check "url on with an empty AWS property name fails" \
  fails helm_url --set config.modelBaseUrlFromSecret=true --set-string externalSecrets.properties.modelBaseUrl=
message=$(helm_url --set config.modelBaseUrlFromSecret=true --set externalSecrets.enabled=false 2>&1 || true)
check "url on with externalSecrets.enabled=false fails" \
  fails helm_url --set config.modelBaseUrlFromSecret=true --set externalSecrets.enabled=false
check "... the error names externalSecrets.enabled" says "$message" 'externalSecrets.enabled'
check "url on with MODEL_BASE_URL in config.extraEnv fails (the chart sets it)" \
  fails helm_url --set config.modelBaseUrlFromSecret=true --set-string config.extraEnv.MODEL_BASE_URL=https://gw.example.com/v1
check "url off with MODEL_BASE_URL in config.extraEnv is unchanged (the deployment's own business)" \
  helm_url --set-string config.extraEnv.MODEL_BASE_URL=https://gw.example.com/v1
check "url on as a string fails (a bool is a bool)" fails helm_url --set-string config.modelBaseUrlFromSecret=false
check "url on as a word fails" fails helm_url --set-string config.modelBaseUrlFromSecret=yes
check "url off needs neither the ExternalSecret property nor the option's other values" \
  helm_url --set externalSecrets.properties.modelBaseUrl=null --set config.modelBaseUrl=https://gw.example.com/v1

# An existing database: database.enabled=false and database.existingSecret. No Cluster is
# rendered, and every pod that sets DATABASE_URL reads it from that Secret and key.
# dburl prints "<secret> <key>" for each DATABASE_URL of the render.
dburl() {
  awk '/name: DATABASE_URL$/ { on = 1; next }
       on && /^ *name: / { n = $2 }
       on && /^ *key: / { print n " " $2; on = 0 }' "$out"
}
dburl_is() { # dburl_is <number of pods> <secret> <key>: that many pods, all reading it
  [ "$(dburl | wc -l | tr -d ' ')" -eq "$1" ] && [ "$(dburl | sort -u)" = "$2 $3" ]
}
helm_db() { helm template coder "$chart" --namespace coder-ns --set image.tag=sha-abc1234 "$@"; }
extdb='--set database.enabled=false --set database.existingSecret.name=coder-db-uri'

helm_db > "$out"
check "database default: the CNPG Cluster and its app Secret, key uri" dburl_is 1 coder-db-app uri
check "database default: no existing Secret is named" lacks 'coder-db-uri'
# shellcheck disable=SC2086
helm_db $extdb > "$out"
check "existing database: no CNPG Cluster is rendered" lacks '^kind: Cluster$'
check "existing database: DATABASE_URL is the named Secret, key uri by default" dburl_is 1 coder-db-uri uri
check "existing database: the CNPG app Secret is not referenced" lacks 'coder-db-app'
# shellcheck disable=SC2086
helm_db $extdb --set database.existingSecret.key=DB_URI > "$out"
check "existing database: the key is configurable" dburl_is 1 coder-db-uri DB_URI
# shellcheck disable=SC2086
helm_db $extdb --set database.existingSecret.key= > "$out"
check "existing database: an empty key falls back to uri" dburl_is 1 coder-db-uri uri
# shellcheck disable=SC2086
helm_db $extdb --set topology=split > "$out"
check "existing database, split: no Cluster" lacks '^kind: Cluster$'
check "existing database, split: the front and the worker both read it" dburl_is 2 coder-db-uri uri
check "existing database, split: the Deployment reads it" \
  [ "$(doc Deployment | awk '/name: DATABASE_URL$/{on=1} on&&/name: coder-db-uri$/{print "y"; exit}')" = y ]
check "existing database, split: the StatefulSet reads it" \
  [ "$(doc StatefulSet | awk '/name: DATABASE_URL$/{on=1} on&&/name: coder-db-uri$/{print "y"; exit}')" = y ]
# shellcheck disable=SC2086
helm_db $extdb --set topology=split --set front.replicas=2 --set database.existingSecret.key=DB_URI > "$out"
check "existing database, split with two fronts: both workloads read the configured key" dburl_is 2 coder-db-uri DB_URI
# shellcheck disable=SC2086
helm_db $extdb --set config.role=control-plane > "$out"
check "existing database, control-plane role: DATABASE_URL is the Secret" dburl_is 1 coder-db-uri uri
# shellcheck disable=SC2086
helm_db $extdb --set config.role=worker > "$out"
check "existing database, worker role: DATABASE_URL is the Secret" dburl_is 1 coder-db-uri uri
# shellcheck disable=SC2086
helm_db $extdb --set workspace.placement=shared --set workspace.sharedVolume.storageClass=rwx --set replicaCount=2 > "$out"
check "existing database, shared placement with two workers: DATABASE_URL is the Secret" dburl_is 1 coder-db-uri uri
# shellcheck disable=SC2086
helm_db $extdb --set networkPolicy.enabled=true --set topology=split > "$out"
check "existing database, with a NetworkPolicy: no Cluster, still one DATABASE_URL per workload" dburl_is 2 coder-db-uri uri
check "existing database, with a NetworkPolicy: egress stays unrestricted (the database needs none)" \
  lacks '^    - Egress$'

message=$(helm_db --set database.enabled=false 2>&1 || true)
check "database off with no Secret fails" fails helm_db --set database.enabled=false
check "... the error names database.existingSecret.name" says "$message" 'database.existingSecret.name'
check "database off with a blank Secret name fails" fails helm_db --set database.enabled=false --set-string 'database.existingSecret.name= '
check "database off with no Secret fails in split too" \
  fails helm_db --set database.enabled=false --set topology=split
message=$(helm_db --set database.existingSecret.name=coder-db-uri 2>&1 || true)
check "database on with an existing Secret named fails (one or the other)" \
  fails helm_db --set database.existingSecret.name=coder-db-uri
check "... the error names both values" says "$message" 'database.enabled=true and database.existingSecret.name'

# ---------------------------------------------------------------------------------------------------------------
# Run pods (runPods, ADR 0019): a pod of its own for each active run. Off by default and then invisible (the golden
# above); on, the chart renders the pod template, the priority class and the quota scoped to it, the RBAC, the
# admission policy that is the guard, and the network policy, and refuses what would leave the coder able to make any pod.
# ---------------------------------------------------------------------------------------------------------------
# rdoc <Kind> <name> [file]: the one document of that kind and metadata name.
rdoc() {
  awk -v k="$1" -v n="$2" '
    function flush() { if (buf ~ ("(^|\n)kind: " k "\n") && buf ~ ("\n  name: " n "\n")) printf "%s", buf; buf = "" }
    /^---$/ { flush(); next }
    { buf = buf $0 "\n" } END { flush() }' "${3:-$out}"
}
rhas() { rdoc "$1" "$2" | grep -Eq -- "$3"; }
rlacks() { ! rdoc "$1" "$2" | grep -Eq -- "$3"; }
helm_rp() { helm template coder "$chart" --namespace coder-ns --set image.tag=sha-abc1234 --set runPods.enabled=true "$@"; }
run_pod_template() { doc ConfigMap | awk '/^  pod.yaml: \|$/ { on = 1; next } on { sub(/^    /, ""); print }'; }
tpl() { run_pod_template | grep -Eq -- "$1"; }
notpl() { ! run_pod_template | grep -Eq -- "$1"; }
sts_of() { doc StatefulSet | grep -Eq -- "$1"; }
nosts() { ! doc StatefulSet | grep -Eq -- "$1"; }
policy_has() { rdoc ValidatingAdmissionPolicy "$1" | grep -Fq -- "$2"; }

helm template coder "$chart" --namespace coder-ns --set image.tag=sha-abc1234 > "$out"
check "run pods off: no run pod template, ServiceAccount, Role, PriorityClass, quota or admission policy" \
  lacks '^kind: (ConfigMap|ServiceAccount|Role|RoleBinding|PriorityClass|ResourceQuota|ValidatingAdmissionPolicy|ValidatingAdmissionPolicyBinding)$'
check "run pods off: none of RUN_ENVIRONMENT, RUN_POD_*, a service account or a mounted token" \
  lacks 'RUN_ENVIRONMENT|RUN_POD_|serviceAccountName|automountServiceAccountToken: true'
check "run pods off: the coder keeps its 6Gi limit" has 'memory: 6Gi'
helm template coder "$chart" --namespace coder-ns --set image.tag=sha-abc1234 --set runPods.enabled=false > "$out"
check "run pods off, set explicitly: the render equals the golden" cmp -s "$out" "$golden"

helm_rp > "$out"
rp_golden="$chart/tests/golden/run-pods.yaml"
check "run pods on: the render equals tests/golden/run-pods.yaml" cmp -s "$out" "$rp_golden"

# Each object, once.
check "run pods: one PriorityClass" count '^kind: PriorityClass$' 1
check "run pods: one ResourceQuota" count '^kind: ResourceQuota$' 1
check "run pods: one Role and one RoleBinding" count '^kind: (Role|RoleBinding)$' 2
check "run pods: one ServiceAccount" count '^kind: ServiceAccount$' 1
check "run pods: one ConfigMap (the pod template)" count '^kind: ConfigMap$' 1
check "run pods: one ValidatingAdmissionPolicy and its binding" count '^kind: ValidatingAdmissionPolicy(Binding)?$' 2
check "run pods: two NetworkPolicies, the coder's and the run pods'" count '^kind: NetworkPolicy$' 2
check "run pods: no Secret object (the model key stays in the ExternalSecret's Secret)" lacks '^kind: Secret$'

# The pod template: what a run pod is.
check "template: apiVersion v1 kind Pod" tpl '^kind: Pod$'
check "template: no metadata name, generateName or ownerReferences" notpl '^  (name|generateName|ownerReferences|namespace):'
check "template: its own name label, not the coder's (the StatefulSet and its NetworkPolicy must never match a run pod)" \
  tpl '^    app.kubernetes.io/name: coder-run$'
check "template: the run container is named run" tpl '^    - name: run$'
check "template: the workspace image by tag AND digest" \
  tpl 'image: "ghcr.io/vymalo/another-agentic-images/workspace:1.98.1-cfd2917@sha256:ee7e4c7539ad62942e43f9b8ea39d8433617888cac3b0d26a37de3993671786c"'
check "template: the memory limit is 2Gi" tpl '^        limits:$'
check "template: memory: 2Gi" tpl '^          memory: 2Gi$'
check "template: requests 250m and 512Mi" tpl '^          cpu: 250m$'
check "template: requests memory 512Mi" tpl '^          memory: 512Mi$'
check "template: CARGO_BUILD_JOBS=2" tpl 'name: CARGO_BUILD_JOBS'
check "template: ... with the value 2" tpl 'value: "2"'
check "template: the priority class is the chart's own, <release>-run" tpl '^  priorityClassName: coder-run$'
check "template: no service account token" tpl '^  automountServiceAccountToken: false$'
check "template: runs as uid 10001, non-root" tpl 'runAsUser: 10001'
check "template: allowPrivilegeEscalation false and every capability dropped" tpl 'allowPrivilegeEscalation: false'
check "template: tini holds the container open" tpl 'command: \["tini", "--", "sleep", "infinity"\]'
check "template: the tools directory is mounted read-only at /opt/adam/bin" tpl 'mountPath: /opt/adam/bin'
check "template: an init container copies adam-exec and OpenCode out of the coder image" \
  tpl 'cp -L /opt/adam/bin/adam-exec /opt/adam/bin/opencode /tools/'
check "template: the init container is the coder's image" tpl 'image: "ghcr.io/vymalo/another-adam-rs/coder:sha-abc1234"'
check "template: /work is the coder's claim, the per-pod one" tpl 'claimName: work-coder-0'
check "template: required pod affinity to the coder pod, by hostname (a ReadWriteOnce volume is attached to one node)" \
  tpl 'requiredDuringSchedulingIgnoredDuringExecution'
check "template: ... topologyKey kubernetes.io/hostname" tpl 'topologyKey: kubernetes.io/hostname'
check "template: ... selecting the coder pods" tpl 'app.kubernetes.io/instance: coder'
check "template: the model key comes from the ExternalSecret's Secret, by key" tpl 'name: MODEL_API_KEY'
check "template: ... as secretKeyRef coder / MODEL_API_KEY" tpl 'key: MODEL_API_KEY'
check "template: no GitHub token, database URL or bearer token" notpl 'GITHUB_TOKEN|GITHUB_APP|DATABASE_URL|A2A_BEARER|SEARCH_MCP_TOKEN|CONTEXT7'
check "template: no hostPath, hostNetwork, hostPID, hostIPC, privileged" notpl 'hostPath|hostNetwork|hostPID|hostIPC|privileged: true'
check "template: no Secret volume" notpl '^      secret:'

# The coder pod.
check "coder: its own ServiceAccount, with the token mounted" sts_of '^      serviceAccountName: coder$'
check "coder: ... automountServiceAccountToken true (for the coder pod alone)" sts_of '^      automountServiceAccountToken: true$'
check "coder: RUN_ENVIRONMENT=kubernetes" sts_of 'value: kubernetes'
check "coder: the template file is the mounted one" sts_of 'value: "/etc/adam/run-pod/pod.yaml"'
check "coder: the namespace is the pod's own (downward API)" sts_of 'fieldPath: metadata.namespace'
check "coder: RUN_POD_INSTANCE is the release" sts_of 'name: RUN_POD_INSTANCE'
check "coder: RUN_POD_IDLE_SECS is 900" sts_of 'name: RUN_POD_IDLE_SECS'
check "coder: RUN_POD_WAIT_SECS is 600 and RUN_POD_READY_TIMEOUT_SECS is 600" sts_of 'name: RUN_POD_WAIT_SECS'
check "coder: WORKER_ID is the pod name (the idle sweep is a worker's own)" sts_of 'name: WORKER_ID'
check "coder: the template is mounted read-only" sts_of 'name: run-pod'
check "coder: a changed template is a deploy (checksum annotation)" sts_of 'checksum/run-pod:'
check "coder: shrinks to a 1Gi limit while run pods are on" sts_of '^              memory: 1Gi$'
check "coder: ... and the 6Gi of resources is not rendered" nosts 'memory: 6Gi'
check "coder: the DATABASE_URL and the secrets stay in the coder" sts_of 'name: GITHUB_TOKEN'

# The ServiceAccount, the Role and the binding.
check "rbac: the Role may make, find, watch and delete pods" rhas Role coder-run-pods 'resources: \["pods"\]'
check "rbac: ... verbs create, delete, get, list, watch" rhas Role coder-run-pods 'verbs: \["create", "delete", "get", "list", "watch"\]'
check "rbac: ... and exec in them" rhas Role coder-run-pods 'resources: \["pods/exec"\]'
check "rbac: ... the exec verbs" rhas Role coder-run-pods 'verbs: \["create", "get"\]'
check "rbac: ... and nothing else: no secrets, configmaps or other kind" rlacks Role coder-run-pods 'secrets|configmaps|deployments|"\*"'
check "rbac: the binding is to the coder's ServiceAccount in the release namespace" rhas RoleBinding coder-run-pods 'name: coder$'
check "rbac: ... in coder-ns" rhas RoleBinding coder-run-pods 'namespace: coder-ns'
check "rbac: the ServiceAccount mounts no token itself (the coder pod asks for it)" rhas ServiceAccount coder 'automountServiceAccountToken: false'

# The quota is scoped by priority class, because a quota cannot select pods by label.
check "quota: the PriorityClass is value 0, preemptionPolicy Never, not the default" rhas PriorityClass coder-run '^value: 0$'
check "quota: ... preemptionPolicy Never" rhas PriorityClass coder-run '^preemptionPolicy: Never$'
check "quota: ... globalDefault false" rhas PriorityClass coder-run '^globalDefault: false$'
check "quota: limits.memory 8Gi" rhas ResourceQuota coder-run-pods 'limits.memory: "8Gi"'
check "quota: pods 4" rhas ResourceQuota coder-run-pods 'pods: "4"'
check "quota: scoped by PriorityClass In [coder-run]" rhas ResourceQuota coder-run-pods 'scopeName: PriorityClass'
check "quota: ... to the chart's class" rhas ResourceQuota coder-run-pods '^          - coder-run$'

# The admission policy: the guard that lets the coder make pods.
pn=coder-coder-ns-run-pods
check "policy: matched to pods created by the coder's ServiceAccount" policy_has "$pn" "request.userInfo.username == 'system:serviceaccount:coder-ns:coder'"
check "policy: fails closed" rhas ValidatingAdmissionPolicy "$pn" 'failurePolicy: Fail'
check "policy: only CREATE of pods" rhas ValidatingAdmissionPolicy "$pn" 'operations: \["CREATE"\]'
check "policy: refuses a pod without the run label and the managed-by label" policy_has "$pn" "'adam.vymalo.com/run' in object.metadata.labels"
check "policy: ... or without the priority class" policy_has "$pn" "object.spec.priorityClassName == 'coder-run'"
check "policy: ... or with another image (the run image by digest and the coder's)" \
  policy_has "$pn" "c.image in ['ghcr.io/vymalo/another-agentic-images/workspace:1.98.1-cfd2917@sha256:ee7e4c7539ad62942e43f9b8ea39d8433617888cac3b0d26a37de3993671786c', 'ghcr.io/vymalo/another-adam-rs/coder:sha-abc1234']"
check "policy: ... or a hostPath: only emptyDir, the work claim and the one Secret" \
  policy_has "$pn" "has(v.emptyDir) || (has(v.persistentVolumeClaim) && v.persistentVolumeClaim.claimName == 'work-coder-0') || (has(v.secret) && v.secret.secretName == 'coder')"
check "policy: ... or a Secret other than the allowed one, by key" policy_has "$pn" "e.valueFrom.secretKeyRef.name == 'coder'"
check "policy: ... or envFrom a Secret" policy_has "$pn" '!has(f.secretRef)'
check "policy: ... or a privileged container or one that may escalate" policy_has "$pn" 'c.securityContext.allowPrivilegeEscalation == false'
check "policy: ... or root" policy_has "$pn" ') != 0 &&'
check "policy: ... or hostNetwork, hostPID, hostIPC" policy_has "$pn" '!object.spec.hostNetwork'
check "policy: ... or a service account token" policy_has "$pn" 'object.spec.automountServiceAccountToken == false'
check "policy: nine rules" [ "$(rdoc ValidatingAdmissionPolicy "$pn" | grep -c -- '^    - expression:')" -eq 9 ]
check "policy: the binding denies" rhas ValidatingAdmissionPolicyBinding "$pn" 'validationActions: \["Deny"\]'
check "policy: ... for this namespace only" rhas ValidatingAdmissionPolicyBinding "$pn" 'kubernetes.io/metadata.name: coder-ns'

# The network policy of the run pods.
rn=coder-run-pods
check "netpol: selects the coder's run pods by label" rhas NetworkPolicy "$rn" 'app.kubernetes.io/managed-by: adam-coder'
check "netpol: ... of this release" rhas NetworkPolicy "$rn" 'app.kubernetes.io/instance: coder'
check "netpol: restricts ingress and egress" rhas NetworkPolicy "$rn" '^    - Egress$'
check "netpol: no ingress at all" rhas NetworkPolicy "$rn" '^  ingress: \[\]$'
check "netpol: DNS on 53, UDP and TCP" rhas NetworkPolicy "$rn" 'port: 53'
check "netpol: the internet, 0.0.0.0/0 ..." rhas NetworkPolicy "$rn" 'cidr: 0.0.0.0/0'
check "netpol: ... except RFC 1918" rhas NetworkPolicy "$rn" '^              - 10.0.0.0/8$'
check "netpol: ... and link-local (cloud metadata)" rhas NetworkPolicy "$rn" '^              - 169.254.0.0/16$'
check "the coder's own NetworkPolicy still restricts ingress only" rlacks NetworkPolicy coder '^    - Egress$'

# Values drive it.
helm_rp --set 'runPods.networkPolicy.clusterCIDRs={10.42.0.0/16,10.43.0.0/16}' > "$out"
check "netpol: the pod and service CIDRs are values-driven" rhas NetworkPolicy "$rn" '^              - 10.43.0.0/16$'
helm_rp --set runPods.networkPolicy.enabled=false > "$out"
check "netpol: off: only the coder's own NetworkPolicy" count '^kind: NetworkPolicy$' 1
helm_rp --set 'runPods.networkPolicy.extraEgress[0].to[0].ipBlock.cidr=10.9.9.9/32' > "$out"
check "netpol: extra egress rules are appended (a model gateway in the cluster)" rhas NetworkPolicy "$rn" 'cidr: 10.9.9.9/32'

helm_rp --set runPods.resources.limits.memory=3Gi --set runPods.resources.requests.memory=1Gi \
  --set runPods.cargoBuildJobs=4 --set runPods.extraEnv.RUSTFLAGS=-Cdebuginfo=0 > "$out"
check "values: the run pod's memory limit" tpl '^          memory: 3Gi$'
check "values: ... its request" tpl '^          memory: 1Gi$'
check "values: ... CARGO_BUILD_JOBS" tpl 'value: "4"'
check "values: ... extra environment" tpl 'name: RUSTFLAGS'
helm_rp --set runPods.quota.pods=2 --set runPods.quota.limitsMemory=4Gi --set runPods.idleSecs=60 \
  --set runPods.waitSecs=30 --set runPods.readyTimeoutSecs=900 > "$out"
check "values: the quota" rhas ResourceQuota coder-run-pods 'pods: "2"'
check "values: ... its memory" rhas ResourceQuota coder-run-pods 'limits.memory: "4Gi"'
check "values: the idle timeout" sts_of 'name: RUN_POD_IDLE_SECS'
helm_rp --set runPods.coderResources=null > "$out"
check "values: no coderResources: the coder keeps its resources" sts_of '^              memory: 6Gi$'
helm_rp --set runPods.coderResources.limits.memory=2Gi > "$out"
check "values: coderResources is values-driven" sts_of '^              memory: 2Gi$'
helm_rp --set runPods.priorityClassName=my-class > "$out"
check "priority class named: the chart renders none" lacks '^kind: PriorityClass$'
check "priority class named: the template" tpl '^  priorityClassName: my-class$'
check "priority class named: the quota" rhas ResourceQuota coder-run-pods '^          - my-class$'
check "priority class named: the policy" policy_has "$pn" "object.spec.priorityClassName == 'my-class'"
helm_rp --set runPods.serviceAccount.name=my-sa > "$out"
check "service account named: the chart renders none" lacks '^kind: ServiceAccount$'
check "service account named: the coder pod uses it" sts_of '^      serviceAccountName: my-sa$'
check "service account named: the binding is to it" rhas RoleBinding coder-run-pods 'name: my-sa$'
check "service account named: the policy is matched to it" policy_has "$pn" "system:serviceaccount:coder-ns:my-sa"
helm_rp --set 'runPods.admissionPolicy.extraAllowedImages={registry.example/extra:1}' > "$out"
check "policy: extra allowed images are values-driven" policy_has "$pn" "'registry.example/extra:1'"
helm_rp --set runPods.toolsImage.repository=registry.example/tools --set runPods.toolsImage.tag=9 > "$out"
check "values: the tools image" tpl 'image: "registry.example/tools:9"'
check "values: ... and the policy allows it" policy_has "$pn" "'registry.example/tools:9'"
helm_rp --set runPods.nodeSelector.pool=builds --set 'runPods.tolerations[0].key=builds' \
  --set 'runPods.nodeAffinity.requiredDuringSchedulingIgnoredDuringExecution.nodeSelectorTerms[0].matchExpressions[0].key=pool' \
  --set 'runPods.nodeAffinity.requiredDuringSchedulingIgnoredDuringExecution.nodeSelectorTerms[0].matchExpressions[0].operator=Exists' > "$out"
check "values: node selector, tolerations and node affinity are in the template" tpl 'pool: builds'
check "values: ... node affinity" tpl 'nodeAffinity:'
check "values: ... together with the pod affinity to the coder" tpl 'podAffinity:'

# The shared volume: no pod affinity, the shared claim.
shared='--set workspace.placement=shared --set workspace.sharedVolume.storageClass=rwx'
# shellcheck disable=SC2086
helm_rp $shared --set replicaCount=3 > "$out"
check "shared volume: the run pod mounts the shared claim" tpl 'claimName: coder-work'
check "shared volume: no pod affinity (every node can mount a ReadWriteMany volume)" notpl 'podAffinity'
check "shared volume: the policy allows that claim" policy_has "$pn" "v.persistentVolumeClaim.claimName == 'coder-work'"
# shellcheck disable=SC2086
helm_rp --set workspace.placement=affinity --set workspace.sharedVolume.storageClass=rwx --set replicaCount=2 > "$out"
check "affinity placement: two replicas are fine with run pods" count '^  replicas: 2$' 1
helm_rp --set workspace.placement=shared --set workspace.sharedVolume.existingClaim=mine > "$out"
check "an existing claim is the one the run pod mounts" tpl 'claimName: mine'

# A control plane starts no commands: nothing of it.
helm_rp --set config.role=control-plane > "$out"
check "control plane: no run pod objects" lacks '^kind: (ConfigMap|ServiceAccount|Role|RoleBinding|PriorityClass|ResourceQuota|ValidatingAdmissionPolicy|ValidatingAdmissionPolicyBinding)$'
check "control plane: no RUN_ENVIRONMENT and no service account" lacks 'RUN_ENVIRONMENT|serviceAccountName'
helm_rp --set topology=split > "$out"
check "split: the worker has the service account" sts_of '^      serviceAccountName: coder$'
check "split: the front has none and mounts no token" [ "$(doc Deployment | grep -c 'serviceAccountName')" -eq 0 ]
check "split: ... automountServiceAccountToken false on the front" dhas Deployment 'automountServiceAccountToken: false'

# Refusals.
check "refused: runPods.enabled that is not a boolean" fails helm_rp --set-string runPods.enabled=maybe
message=$(helm_rp --set runPods.admissionPolicy.enabled=false 2>&1 || true)
check "refused: no admission policy unless acknowledged" fails helm_rp --set runPods.admissionPolicy.enabled=false
check "... the error says why" says "$message" 'disableAcknowledged'
helm_rp --set runPods.admissionPolicy.enabled=false --set runPods.admissionPolicy.disableAcknowledged=true > "$out"
check "acknowledged: no admission policy is rendered" lacks '^kind: ValidatingAdmissionPolicy'
check "acknowledged: the rest is" has '^kind: ResourceQuota$'
message=$(helm_rp --kube-version 1.29.0 2>&1 || true)
check "refused: Kubernetes before 1.30 (no ValidatingAdmissionPolicy)" fails helm_rp --kube-version 1.29.0
check "... the error names 1.30" says "$message" '1.30'
check "Kubernetes 1.30 is enough" helm_rp --kube-version 1.30.0
message=$(helm_rp --set replicaCount=2 2>&1 || true)
check "refused: two workers on per-pod ReadWriteOnce volumes" fails helm_rp --set replicaCount=2
check "... the error says a run pod mounts one claim" says "$message" 'mounts one claim'
check "refused: isolated placement with two workers" fails helm_rp --set workspace.placement=isolated --set replicaCount=2
message=$(helm_rp --set externalSecrets.enabled=false 2>&1 || true)
check "refused: no ExternalSecret (the model key reaches a run pod only from it)" fails helm_rp --set externalSecrets.enabled=false
check "... the error names externalSecrets.enabled" says "$message" 'externalSecrets.enabled'
check "refused: a model key in extraEnv" fails helm_rp --set runPods.extraEnv.MODEL_API_KEY=x
check "refused: a bad image digest" fails helm_rp --set runPods.image.digest=sha256:abc
check "refused: no memory limit (the quota would refuse every pod)" fails helm_rp --set runPods.resources.limits.memory=null
check "refused: a quota that is not a quantity" fails helm_rp --set runPods.quota.limitsMemory=lots
check "refused: no pods in the quota" fails helm_rp --set runPods.quota.pods=0
check "refused: an idle timeout that is not a number" fails helm_rp --set-string runPods.idleSecs=soon
check "refused: a bad container name" fails helm_rp --set runPods.container=Run_Container
check "refused: a CIDR that is not one" fails helm_rp --set 'runPods.networkPolicy.clusterCIDRs={everything}'

# Optional A2A features (a2a.*, ADR 0030 of adam-rs): push notifications and the signature of the agent card.
# Both are off by default and then invisible (the golden above, byte for byte); each belongs to the pod that
# serves A2A. The key is a Secret you manage, mounted, never a value of the chart.
a2a_vars='A2A_PUSH_ALLOWED_URLS|A2A_PUSH_ALLOW_PRIVATE|A2A_PUSH_GIVE_UP_AFTER_SECS|A2A_PUSH_REQUEST_TIMEOUT_SECS|A2A_CARD_SIGNING_'
helm_a2a() { helm template coder "$chart" --namespace coder-ns --set image.tag=sha-abc1234 "$@"; }
helm_a2a > "$out"
check "a2a off: none of its variables, and no signing volume" lacks "$a2a_vars|card-signing"
check "a2a off: the default render is still the golden" cmp -s "$out" "$golden"
helm_a2a --set-json 'a2a.push.allowedUrls=[]' --set a2a.cardSigning.secretName= > "$out"
check "a2a off, set explicitly (empty list, empty name): the render equals the golden" cmp -s "$out" "$golden"

helm_a2a --set 'a2a.push.allowedUrls={https://hooks.example.com/a2a/,*.partner.io}' > "$out"
check "push on: the allow-list is A2A_PUSH_ALLOWED_URLS, joined by commas, in the StatefulSet" \
  dhas StatefulSet 'value: "https://hooks.example.com/a2a/,\*.partner.io"'
check "push on: the delivery bounds are rendered, with their defaults" \
  dhas StatefulSet 'name: A2A_PUSH_GIVE_UP_AFTER_SECS'
check "push on: give up after an hour by default" dhas StatefulSet 'value: "3600"'
check "push on: the request timeout is 15 s by default" dhas StatefulSet 'value: "15"'
check "push on: private addresses stay off (no A2A_PUSH_ALLOW_PRIVATE)" lacks 'A2A_PUSH_ALLOW_PRIVATE'
check "push on: no card-signing variable or volume" lacks 'A2A_CARD_SIGNING_|card-signing'
check "push on: no Secret object and no webhook credential anywhere" lacks '^kind: Secret$'
helm_a2a --set 'a2a.push.allowedUrls={hooks.example.com}' --set a2a.push.giveUpAfterSecs=60 --set a2a.push.requestTimeoutSecs=5 > "$out"
check "push: the bounds are values" dhas StatefulSet 'value: "60"'
helm_a2a --set 'a2a.push.allowedUrls={127.0.0.1:9000}' --set a2a.push.allowPrivateAddresses=true > "$out"
check "push, development switch: A2A_PUSH_ALLOW_PRIVATE is rendered" dhas StatefulSet 'name: A2A_PUSH_ALLOW_PRIVATE'

helm_a2a --set a2a.cardSigning.secretName=coder-card-key > "$out"
check "signing on: A2A_CARD_SIGNING_KEY_FILE names the mounted file" \
  dhas StatefulSet 'value: "/var/run/secrets/card-signing/private-key.pem"'
check "signing on: the Secret is mounted read-only" dhas StatefulSet 'mountPath: /var/run/secrets/card-signing'
check "signing on: ... from the Secret that was named, key private-key.pem" dhas StatefulSet 'secretName: coder-card-key'
check "signing on: ... group-readable only (0440)" dhas StatefulSet 'defaultMode: 0440'
check "signing on: no key id and no jku unless set" lacks 'A2A_CARD_SIGNING_KEY_ID|A2A_CARD_SIGNING_JKU'
check "signing on: push stays off" lacks 'A2A_PUSH_'
helm_a2a --set a2a.cardSigning.secretName=coder-card-key --set a2a.cardSigning.keyId=key-1 \
  --set a2a.cardSigning.jku=https://coder.example.com/.well-known/jwks.json > "$out"
check "signing: keyId and jku are rendered when set" \
  dhas StatefulSet 'value: "https://coder.example.com/.well-known/jwks.json"'
check "signing: ... the key id too" dhas StatefulSet 'name: A2A_CARD_SIGNING_KEY_ID'

# topology=split: only the front serves A2A, so only the front has them; the worker has none of it.
helm_a2a --set topology=split --set 'a2a.push.allowedUrls={hooks.example.com}' --set a2a.cardSigning.secretName=coder-card-key > "$out"
check "split: the front has the allow-list" dhas Deployment 'name: A2A_PUSH_ALLOWED_URLS'
check "split: the front has the signing key mounted" dhas Deployment 'mountPath: /var/run/secrets/card-signing'
check "split: the worker has no A2A variable of these" dlacks StatefulSet "$a2a_vars"
check "split: the worker does not mount the key" dlacks StatefulSet 'card-signing'
check "split with a2a on: the render equals tests/golden/a2a-split.yaml" cmp -s "$out" "$chart/tests/golden/a2a-split.yaml"
helm_a2a --set topology=split > "$out"
check "split, a2a off: the front has none of it either" lacks "$a2a_vars|card-signing"

# Refusals: a mistake stops the render, not a rollout.
check "refused: allowedUrls that is not a list" fails helm_a2a --set a2a.push.allowedUrls=hooks.example.com
check "refused: an entry with a space or a comma" fails helm_a2a --set 'a2a.push.allowedUrls={a.example.com b.example.com}'
check "refused: an empty entry" fails helm_a2a --set-json 'a2a.push.allowedUrls=[""]'
check "refused: plain http without the development switch" fails helm_a2a --set 'a2a.push.allowedUrls={http://hooks.example.com/}'
helm_a2a --set 'a2a.push.allowedUrls={http://127.0.0.1:9000/}' --set a2a.push.allowPrivateAddresses=true > "$out"
check "plain http is allowed beside the development switch" dhas StatefulSet 'name: A2A_PUSH_ALLOW_PRIVATE'
message=$(helm_a2a --set a2a.push.allowPrivateAddresses=true 2>&1 || true)
check "refused: the development switch without an allow-list" fails helm_a2a --set a2a.push.allowPrivateAddresses=true
check "... the error says it has no effect" says "$message" 'no effect'
check "refused: allowPrivateAddresses that is not a boolean" \
  fails helm_a2a --set 'a2a.push.allowedUrls={hooks.example.com}' --set-string a2a.push.allowPrivateAddresses=maybe
check "refused: a give-up bound of 0" fails helm_a2a --set 'a2a.push.allowedUrls={hooks.example.com}' --set a2a.push.giveUpAfterSecs=0
check "refused: a give-up bound beyond a week" \
  fails helm_a2a --set 'a2a.push.allowedUrls={hooks.example.com}' --set a2a.push.giveUpAfterSecs=604801
check "refused: a request timeout beyond 120 s" \
  fails helm_a2a --set 'a2a.push.allowedUrls={hooks.example.com}' --set a2a.push.requestTimeoutSecs=121
message=$(helm_a2a --set a2a.cardSigning.keyId=key-1 2>&1 || true)
check "refused: a key id without a key" fails helm_a2a --set a2a.cardSigning.keyId=key-1
check "... the error names the Secret" says "$message" 'secretName'
check "refused: a jku without a key" fails helm_a2a --set a2a.cardSigning.jku=https://x.example/jwks.json
check "refused: a Secret name that is not one" fails helm_a2a --set a2a.cardSigning.secretName=Not_A_Secret
check "refused: the allow-list in extraEnv beside the value" \
  fails helm_a2a --set 'a2a.push.allowedUrls={hooks.example.com}' --set-string config.extraEnv.A2A_PUSH_ALLOWED_URLS=x

if [ "$fail" -eq 0 ]; then echo "render checks passed"; else echo "render checks FAILED"; exit 1; fi
