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

if [ "$fail" -eq 0 ]; then echo "render checks passed"; else echo "render checks FAILED"; exit 1; fi
