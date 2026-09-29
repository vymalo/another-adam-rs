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

helm template coder "$chart" --namespace coder-ns --set image.tag=sha-abc1234 > "$out"

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
workspace_and_checks='name: (WORKSPACE_ROOT|WORKERS|MAX_CHECK_CYCLES|CHECK_TIMEOUT_SECS|ALLOWED_REPO_HOSTS|GITHUB_API_URL|PR_DRAFT|GIT_AUTHOR_NAME|GIT_AUTHOR_EMAIL)$'
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

# The roles that run workers keep all of it, and their two secrets are required.
for role in "" all worker; do
  if [ -n "$role" ]; then set -- --set "config.role=$role"; else set --; fi
  label=${role:-default}
  helm template coder "$chart" --namespace coder-ns "$@" > "$out"
  check "the $label role gets the model, GitHub and OpenCode settings" count "$model_and_github" 5
  check "the $label role gets the workspace and check settings" count "$workspace_and_checks" 9
  check "the $label role gets both worker secrets, in the pod and in the ExternalSecret" count "$secrets_of_workers" 2
  check "the $label role reads both worker secrets from the Secret" count '^                  key: (MODEL_API_KEY|GITHUB_TOKEN)$' 2
  for property in modelApiKey githubToken; do
    check "the $label role fails to render without externalSecrets.properties.$property" \
      fails helm template coder "$chart" --namespace coder-ns "$@" --set "externalSecrets.properties.$property=null"
  done
done

if [ "$fail" -eq 0 ]; then echo "render checks passed"; else echo "render checks FAILED"; exit 1; fi
