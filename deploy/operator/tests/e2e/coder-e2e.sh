#!/bin/sh
# The kind end-to-end of the coder (S9): the operator, installed from its chart, runs the REAL adam-rs coder image.
#
#   sh deploy/operator/tests/e2e/coder-e2e.sh        (from the repository root, kubectl pointed at a kind cluster)
#
# What it does, in order, and asserts as it goes:
#   1. the CRDs (deploy/operator-crds, server-side, as Argo CD does) and a Postgres, dummy Secrets for the coder;
#   2. the operator chart, with the image of the job (OPERATOR_IMAGE_*): the operator is Ready, runs as 65532 and its
#      service account may do what its Role says and nothing else (`kubectl auth can-i`);
#   3. examples/coder.yaml as adapted in coder.yaml: the AgentService becomes Ready, the coder pod is Ready, its agent card
#      answers through the Service (from a pod), its JSON-RPC endpoint refuses a request with no token, and the registry
#      lists it, with the token (a pod that may not read the registry is refused, when the cluster enforces NetworkPolicies);
#   4. the AgentService is deleted: the finalizer completes, the StatefulSet and Service go, and the claim stays (Retain).
#
# What it does NOT prove: no model (MODEL_BASE_URL is never called), no GitHub (a dummy token, and the GitHub MCP server of the
# pod is only asked for its tool list, which makes no request to GitHub), no task is sent to the coder, no GitHub App.
#
# Needs kubectl, helm, jq, openssl and a cluster that has the operator image already (kind load docker-image). Every step is
# bounded; on a failure the cluster's state is printed.
#
# OPERATOR_IMAGE_REPOSITORY (default operator), OPERATOR_IMAGE_TAG (default smoke) and OPERATOR_PULL_POLICY (default Never)
# say which image the chart runs: the one loaded into kind, never one pulled.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../../../.." && pwd)
ns=another-agentic-system
sa="system:serviceaccount:$ns:adam-operator"
image_repository=${OPERATOR_IMAGE_REPOSITORY:-operator}
image_tag=${OPERATOR_IMAGE_TAG:-smoke}
pull_policy=${OPERATOR_PULL_POLICY:-Never}
# The coder image is 2.9 GB compressed: the first pull is most of the wait.
ready_timeout=${READY_TIMEOUT_SECS:-1500}
work=$(mktemp -d)
curl_image='docker.io/curlimages/curl:8.22.0@sha256:58adaa4e8dca9c988bae2aba4ab3434a0bb2da16bbe3f92dec39ec7785166777'

step() { printf '\n== %s\n' "$1"; }
ok() { echo "ok   $1"; }
die() { echo "FAIL $1" >&2; exit 1; }

diagnostics() {
  echo "::group::what the cluster says"
  kubectl -n "$ns" get agentservices,agentconfigs,deployments,statefulsets,services,networkpolicies,pvc,pods -o wide || true
  kubectl -n "$ns" get agentservice coder -o yaml | tail -n 80 || true
  kubectl -n "$ns" get events --sort-by=.lastTimestamp | tail -n 60 || true
  echo "::endgroup::"
  echo "::group::the operator's log"
  kubectl -n "$ns" logs deployment/adam-operator --tail=150 || true
  echo "::endgroup::"
  echo "::group::the coder pod"
  kubectl -n "$ns" describe pod -l app.kubernetes.io/instance=coder | tail -n 120 || true
  kubectl -n "$ns" logs -l app.kubernetes.io/instance=coder -c agent --tail=80 || true
  kubectl -n "$ns" logs -l app.kubernetes.io/instance=coder -c github-mcp --tail=40 || true
  echo "::endgroup::"
}
cleanup() {
  status=$?
  if [ "$status" -ne 0 ]; then diagnostics; fi
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT

# jp <resource> <jsonpath>: a field of an object of the namespace, empty when it is not there.
jp() { kubectl -n "$ns" get "$1" -o "jsonpath=$2" 2>/dev/null || true; }
# until_ok <seconds> <description> <command...>: poll every 5 s.
until_ok() {
  limit=$1; what=$2; shift 2
  deadline=$(( $(date +%s) + limit ))
  while ! "$@" >/dev/null 2>&1; do
    [ "$(date +%s)" -lt "$deadline" ] || die "gave up after ${limit}s waiting for: $what"
    sleep 5
  done
}
# gone <kind/name>...: none of them exists any more.
gone() {
  for r in "$@"; do
    if kubectl -n "$ns" get "$r" >/dev/null 2>&1; then return 1; fi
  done
}
# In the curl pod: the status code of a request (body in /tmp/b of the pod), or 000.
req() { # req <pod> <curl args...>
  pod=$1; shift
  kubectl -n "$ns" exec "$pod" -- curl -s -o /tmp/b -w '%{http_code}' -m 10 "$@" 2>/dev/null || true
}
body() { kubectl -n "$ns" exec "$1" -- cat /tmp/b; }

step "the cluster"
kubectl get nodes
kubectl get namespace "$ns" >/dev/null 2>&1 || kubectl create namespace "$ns"

step "1. the CRDs (server-side apply, as Argo CD syncs them), a Postgres, and the coder's Secrets (dummy)"
helm template adam-operator-crds "$repo/deploy/operator-crds" | kubectl apply --server-side -f -
kubectl wait --for=condition=Established --timeout=60s crd/agentconfigs.agents.vymalo.com crd/agentservices.agents.vymalo.com
ok "the CRDs are installed and Established"
kubectl apply -f "$here/postgres.yaml"
a2a_token=$(openssl rand -hex 24)
registry_token=$(openssl rand -hex 24)
secret() { kubectl -n "$ns" create secret generic "$@" --dry-run=client -o yaml | kubectl apply -f - >/dev/null; }
secret coder-db-uri "--from-literal=uri=postgres://postgres:e2e-throwaway@coder-postgres.$ns.svc:5432/coder"
secret coder-secrets "--from-literal=A2A_BEARER_TOKENS=$a2a_token" --from-literal=MODEL_API_KEY=dummy-model-key \
  --from-literal=GITHUB_TOKEN=dummy-github-token
secret adam-operator-registry-token "--from-literal=token=$registry_token"
kubectl -n "$ns" rollout status deployment/coder-postgres --timeout=240s
ok "Postgres is ready, the Secrets exist"

step "2. the operator chart, with the image of this job"
helm upgrade --install adam-operator "$repo/deploy/operator" --namespace "$ns" \
  -f "$here/operator.values.yaml" \
  --set "image.repository=$image_repository" --set "image.tag=$image_tag" --set "image.pullPolicy=$pull_policy" \
  --wait --timeout 240s
ok "the operator is Ready (/readyz: both caches have listed the cluster)"
[ "$(jp deployment/adam-operator '{.status.readyReplicas}')" = 1 ] || die "the operator has no ready replica"
[ "$(jp deployment/adam-operator '{.spec.replicas}')" = 1 ] || die "the operator has more or fewer than one replica"
pod=$(kubectl -n "$ns" get pod -l app.kubernetes.io/name=adam-operator -o jsonpath='{.items[0].metadata.name}')
[ "$(jp "pod/$pod" '{.spec.securityContext.runAsUser}')" = 65532 ] || die "the operator pod does not run as 65532"
[ "$(jp "pod/$pod" '{.spec.containers[0].securityContext.readOnlyRootFilesystem}')" = true ] || die "the root filesystem is not read-only"
ok "the operator pod runs as 65532 on a read-only root filesystem"
# What the Role allows, asked of the API server as the service account (`--subresource`, because `kubectl auth can-i get x/y` means
# the object y of x). CloudNativePG is not installed here, so its rule is read from the Role instead.
can() { kubectl auth can-i "$@" --as="$sa" 2>/dev/null || true; }
for v in "create statefulsets" "delete deployments" "patch agentservices" "patch agentservices --subresource=status" \
         "watch pods" "create networkpolicies" "create persistentvolumeclaims"; do
  # shellcheck disable=SC2086
  [ "$(can $v -n "$ns")" = yes ] || die "the operator may not: $v"
done
for v in "get secrets" "list secrets" "create secrets" "update agentservices --subresource=finalizers" \
         "patch agentservices --subresource=finalizers" "create clusterroles" "delete pods" "create pods"; do
  # shellcheck disable=SC2086
  [ "$(can $v -n "$ns")" = no ] || die "the operator may: $v"
done
[ "$(can list agentservices --all-namespaces)" = no ] || die "the operator may list agentservices in every namespace"
[ "$(can list pods -n kube-system)" = no ] || die "the operator may list pods in kube-system"
[ "$(kubectl -n "$ns" get role adam-operator -o json | jq -c '[.rules[] | select(.resources == ["clusters"]) | .verbs] | .[0]')" = '["get","patch","create","delete"]' ] \
  || die "the Role has no rule for CloudNativePG clusters (get, patch, create, delete)"
ok "the service account has the Role's rights, none on Secrets, none outside $ns"

step "3. the coder: examples/coder.yaml as adapted (coder.yaml), the real image"
kubectl apply -f "$here/coder.yaml"
# Ready, or a state that will not get better by waiting.
coder_ready() {
  [ "$(jp agentservice/coder '{.status.state}')" = Ready ]
}
start=$(date +%s)
deadline=$((start + ready_timeout))
blocked=0
while ! coder_ready; do
  state=$(jp agentservice/coder '{.status.state}')
  [ "$(date +%s)" -lt "$deadline" ] || die "the coder is not Ready after ${ready_timeout}s (state: ${state:-none})"
  # Blocked is the operator refusing the spec or a missing object: waiting does not help. Three looks in a row (30 s), so the
  # first pass that runs before its AgentConfig is visible is not taken for a refusal.
  if [ "$state" = Blocked ]; then blocked=$((blocked + 1)); else blocked=0; fi
  if [ "$blocked" -ge 3 ]; then
    die "the coder is Blocked: $(jp agentservice/coder '{.status.conditions}')"
  fi
  echo "     $(( $(date +%s) - start ))s: state ${state:-none}, pod: $(kubectl -n "$ns" get pod -l app.kubernetes.io/instance=coder --no-headers 2>/dev/null | head -1 | tr -s ' ' | cut -d' ' -f2-3 || true)"
  sleep 10
done
ok "the AgentService is Ready after $(( $(date +%s) - start ))s"
kubectl -n "$ns" wait pod/coder-0 --for=condition=Ready --timeout=60s
[ "$(jp statefulset/coder '{.spec.template.spec.containers[0].image}')" = "$(sed -n 's/^ *ref: //p' "$here/coder.yaml")" ] \
  || die "the pod does not run the image of coder.yaml"
ok "the coder pod is Ready, on the image of coder.yaml (tag and digest)"
[ "$(jp statefulset/coder '{.spec.template.spec.initContainers[0].name}')" = github-mcp ] || die "no GitHub MCP sidecar"
for c in ConfigResolved StoreReady RuntimeReady Ready; do
  [ "$(jp agentservice/coder "{.status.conditions[?(@.type==\"$c\")].status}")" = True ] || die "condition $c is not True"
done
ok "ConfigResolved, StoreReady, RuntimeReady and Ready are True"
card_url=$(jp agentservice/coder '{.status.endpoints.agentCard}')
[ "$card_url" = "http://coder.$ns.svc:8080/.well-known/agent-card.json" ] || die "status.endpoints.agentCard is '$card_url'"
ok "status.endpoints.agentCard is $card_url"

# Two pods in the namespace to ask from: one the registry's NetworkPolicy lets in, and one it does not.
kubectl -n "$ns" run curl --image="$curl_image" --labels=adam-operator-e2e/client=true --restart=Never --command -- sleep 3600 >/dev/null
kubectl -n "$ns" run stranger --image="$curl_image" --restart=Never --command -- sleep 3600 >/dev/null
kubectl -n "$ns" wait pod/curl pod/stranger --for=condition=Ready --timeout=180s

step "   the card, through the Service, from a pod"
[ "$(req curl "$card_url")" = 200 ] || die "GET $card_url is not 200"
[ "$(body curl | jq -r .name)" = Adam ] || die "the card does not name the agent Adam: $(body curl | head -c 300)"
body curl | jq -e '.skills[] | select(.id == "coding-task")' >/dev/null || die "the card lists no coding-task skill"
ok "the card answers 200 through the Service and names Adam and its skill coding-task"
[ "$(req curl "http://coder.$ns.svc:8080/healthz")" = 200 ] || die "GET /healthz is not 200"
a2a_url=$(jp agentservice/coder '{.status.endpoints.a2a}')
refused=$(req curl -X POST -H 'Content-Type: application/json' -d '{}' "$a2a_url")
[ "$refused" = 401 ] || die "POST $a2a_url with no token is $refused, want 401 (fail closed)"
[ "$(req curl -X POST -H 'Content-Type: application/json' -H 'Authorization: Bearer wrong' -d '{}' "$a2a_url")" = 401 ] \
  || die "a wrong token is not 401"
accepted=$(req curl -X POST -H 'Content-Type: application/json' -H "Authorization: Bearer $a2a_token" -d '{}' "$a2a_url")
case $accepted in 401 | 403) die "the token of coder-secrets was refused ($accepted): the Secret did not reach the pod" ;; esac
ok "the A2A endpoint is closed without the token ($refused), and the token of the Secret opens it ($accepted)"

step "   the registry"
reg="http://adam-operator-registry.$ns.svc:8080/registry/v1/agents"
[ "$(req curl "$reg")" = 401 ] || die "the registry answers a request with no token with something else than 401"
[ "$(req curl -H "Authorization: Bearer $registry_token" -H 'Accept: application/linkset+json' "$reg")" = 200 ] \
  || die "the registry refused its own token"
body curl | jq -e --arg card "$card_url" \
  '.linkset[0].item == [{"href": $card, "type": "application/json", "service": ["coder"], "title": "Coder", "tags": ["coding", "git"]}]' \
  >/dev/null || die "the registry does not list the coder as expected: $(body curl | head -c 600)"
ok "the registry lists the coder (service, title, tags) with its card URL, to the holder of the token, and 401 to nobody"
[ "$(jp agentservice/coder '{.status.conditions[?(@.type=="Listed")].reason}')" = Listed ] || die "Listed is not True / Listed"
# The card the registry lists is the card the pod serves.
href=$(body curl | jq -r '.linkset[0].item[0].href')
[ "$(req curl "$href")" = 200 ] || die "the card the registry lists is not served"
[ "$(body curl | jq -r .name)" = Adam ] || die "the card the registry lists is not the coder's"
ok "the card it lists is the card the pod serves"
# NetworkPolicy: only the labelled pods may read the registry. kind's CNI enforces policies since kindnetd learned to; if it does
# not, say so instead of failing (what this proves is the operator's, not the CNI's).
stranger=$(req stranger -H "Authorization: Bearer $registry_token" "$reg")
if [ "$stranger" = 200 ]; then
  echo "::warning::a pod that the registry's NetworkPolicy does not name read the registry: this cluster does not enforce NetworkPolicies, so the policy is not proven here"
else
  ok "a pod the registry's NetworkPolicy does not name cannot read it (status $stranger)"
fi
# The operator's own egress policy did not stop it from working: it listed the services above.

step "4. delete the AgentService: the finalizer completes, the claim stays (deletionPolicy Retain)"
claim="work-coder-0"
[ -n "$(jp "pvc/$claim" '{.metadata.name}')" ] || die "the claim $claim does not exist before the deletion"
kubectl -n "$ns" delete agentservice coder --wait=true --timeout=300s
ok "the AgentService is gone: its finalizer completed"
until_ok 120 "the StatefulSet and the Service to be gone" gone statefulset/coder service/coder
ok "the StatefulSet and the Service of the coder are gone"
until_ok 120 "the coder pod to be gone" gone pod/coder-0
[ "$(jp "pvc/$claim" '{.metadata.name}')" = "$claim" ] || die "the claim $claim was deleted under Retain"
[ -z "$(jp "pvc/$claim" '{.metadata.ownerReferences}')" ] || die "the retained claim still has an owner ($(jp "pvc/$claim" '{.metadata.ownerReferences}'))"
ok "the claim $claim stays, with no owner: the volume outlives the service"
[ "$(jp agentconfig/coder '{.metadata.name}')" = coder ] || die "the AgentConfig was deleted with the service"
ok "the AgentConfig stays"
# The registry no longer lists it.
[ "$(req curl -H "Authorization: Bearer $registry_token" "$reg")" = 200 ] || die "the registry is not answering"
[ "$(body curl | jq '[.linkset[0].item // [] | .[]] | length')" = 0 ] || die "the deleted coder is still listed: $(body curl | head -c 400)"
ok "the registry lists nothing"
kubectl -n "$ns" delete pod curl stranger --wait=false >/dev/null

printf '\ncoder end-to-end passed\n'
