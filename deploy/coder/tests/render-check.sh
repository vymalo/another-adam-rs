#!/bin/sh
# Assertions on the rendered chart: the properties the design depends on, so a
# careless edit to the templates or values fails CI instead of exposing the
# coder. Needs only `helm`, `grep` and `awk`.
#
#   deploy/coder/tests/render-check.sh            (from the repository root)
set -eu

chart="$(dirname "$0")/.."
out=$(mktemp)
trap 'rm -f "$out"' EXIT
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

if [ "$fail" -eq 0 ]; then echo "render checks passed"; else echo "render checks FAILED"; exit 1; fi
