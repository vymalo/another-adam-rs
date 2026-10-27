#!/bin/sh
# The functions below that until_ok and must_be_refused call by name are not unreachable.
# shellcheck disable=SC2317
# Run pods against a real cluster (ADR 0019, ADR 0042): applies the chart's run-pod objects, the egress proxy and the network
# policies to the cluster kubectl points at (a kind cluster in CI), runs the cluster test of adam-env-kubernetes against it
# **as the coder's ServiceAccount**, so that the chart's admission policy applies to what the test makes, and then shows what
# the network policies do to a run pod.
#
#   kind create cluster --image kindest/node:v1.35.8@sha256:07b2536e30b803ed61d1677a79df6115f798ce64c80f9e22f6ed45afd09323c0
#   sh deploy/coder/tests/kind-run-pods.sh        (from the repository root)
#
# Needs docker (with BuildKit), kubectl, helm, cargo and python3, and a cluster of Kubernetes 1.30 or later (the script stops on
# an older one) whose network plugin **enforces NetworkPolicies**: kind's own, kindnet, does on that node image, and the script
# FAILS where a policy is not enforced. A cluster known not to enforce them (a plugin without NetworkPolicy support) can say so
# with ADAM_TEST_CNI_MAY_IGNORE_POLICIES=1: the checks that a connection is refused then warn instead of failing, and the
# policies are shown to be valid and installed and not to be enforced. What it proves, and what it does not, is in
# deploy/coder/README.md ("Run pods"):
#
#   * the objects the chart renders for runPods apply, the quota and the admission policy take effect, and a run pod
#     made from the chart's own template runs commands, is killed, swept when idle and released (the cluster test);
#   * a run pod sees only its own run (ADR 0041): the volume is seeded with two runs, a mirror and another mirror, and
#     the cluster test makes the pod of one run and looks at what it can see, read and write;
#   * the network policy of run pods is deny by default and the egress proxy is the one way out (ADR 0042), asked of a run
#     pod made from the chart's template and admitted by the admission policy, with the proxy of the chart running
#     `adam-egress` built here:
#       - a control: the proxy's own pod reaches a public address on 443, and a pod no policy selects reaches another pod;
#       - a direct connection from the run pod (curl --noproxy '*') to that public address, to the API server (by its
#         Service name, which DNS resolves, and by its address) and to another pod's IP is refused;
#       - the proxy's pod itself reaches a public address on 443 and not on 80, and no pod of the cluster, and a pod that
#         is not a run pod cannot use the proxy;
#       - through the proxy (the HTTPS_PROXY of the template): `curl` and `npm view` reach registry.npmjs.org, and
#         https://example.com/ is answered 403 by the proxy, which logs the refusal (decision, host) and nothing of the
#         request: a Proxy-Authorization header and a query sent to it are in no log line;
#       - no rule of any policy names an IPv6 range (kind is IPv4 only: IPv6 egress cannot be tried here, only the absence
#         of a rule that would allow it);
#     the refusals are retried for up to 60 s, because kindnet programs a policy a moment after it sees a pod;
#   * not that the coder runs a task over a model through it, and not a cluster whose plugin does not enforce NetworkPolicy.
#
# The image is one small local image (docker/coder/test/run-pod.Dockerfile) that plays the run container, the coder's image for
# the init container, and the coder's image for the egress proxy (with adam-egress built here, npm and curl), loaded into kind.
# NAMESPACE and KIND_NAME can be set.
set -eu

namespace=${NAMESPACE:-coder-ns}
kind_name=${KIND_NAME:-kind}
cni_may_ignore=${ADAM_TEST_CNI_MAY_IGNORE_POLICIES:-}
image=adam-run-test
tag=ci
secret_key=ci-model-key
# The release is `coder`: the proxy is <release>-egress (the chart's coder.egressProxyName).
proxy=coder-egress
proxy_url="http://$proxy.$namespace.svc:3128"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

say() { printf '== %s\n' "$*"; }
ok() { printf 'ok   %s\n' "$*"; }

# What the cluster says: the pods and the policies, the proxy's log (one line per decision, never a path, a query or a
# header), and the network plugin's. Never a Secret and never a token.
diagnostics() {
  say "what the cluster says"
  kubectl get pods,services,endpoints,networkpolicy,resourcequota -n "$namespace" -o wide || true
  kubectl describe pods -n "$namespace" | tail -n 120 || true
  kubectl describe networkpolicy -n "$namespace" || true
  kubectl logs -n "$namespace" -l app.kubernetes.io/component=egress-proxy --all-containers --prefix --tail=60 || true
  kubectl get events -n "$namespace" --sort-by=.lastTimestamp | tail -n 30 || true
  kubectl get pods -n kube-system -o wide || true
  kubectl logs -n kube-system -l app=kindnet --tail=30 || true
  kubectl get validatingadmissionpolicy -o yaml | tail -n 40 || true
}
die() {
  printf 'FAIL %s\n' "$*" >&2
  diagnostics
  exit 1
}
# until_ok <seconds> <description> <command...>: poll every 3 s. The output of the last attempt is what the failure shows.
until_ok() {
  limit=$1; what=$2; shift 2
  deadline=$(( $(date +%s) + limit ))
  while ! last=$("$@" 2>&1); do
    [ "$(date +%s)" -lt "$deadline" ] || die "gave up after ${limit}s waiting for: $what. The last attempt said: $(printf '%s' "$last" | tail -c 700)"
    sleep 3
  done
}

minor=$(kubectl version -o json | python3 -c 'import json,sys; print(json.load(sys.stdin)["serverVersion"]["minor"].rstrip("+"))')
say "the cluster is Kubernetes 1.$minor"
if [ "$minor" -lt 30 ]; then
  echo "the admission policy needs Kubernetes 1.30 or later" >&2
  exit 1
fi

say "adam-egress, built here (the proxy of the chart runs it)"
cargo build --release --locked -p adam-egress
target_dir=$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
mkdir -p "$work/egress"
cp "$target_dir/release/adam-egress" "$work/egress/adam-egress"

say "the image of the run pods and of the proxy, loaded into kind"
docker build -q -f docker/coder/test/run-pod.Dockerfile --build-context "egress=$work/egress" -t "$image:$tag" .
kind load docker-image "$image:$tag" --name "$kind_name"

say "the namespace, the volume the run pods mount at /work, and the model key's Secret"
kubectl create namespace "$namespace" --dry-run=client -o yaml | kubectl apply -f -
kubectl apply -n "$namespace" -f - <<YAML
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: work-test
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 1Gi
YAML
# The Secret the chart's ExternalSecret would make (<release>-coder is `coder` for the release `coder`).
kubectl create secret generic coder -n "$namespace" --from-literal=MODEL_API_KEY="$secret_key" \
  --dry-run=client -o yaml | kubectl apply -f -

say "the chart's run-pod objects and the egress proxy"
# image.* is the coder's image, which the egress proxy runs too: the local image has adam-egress in it.
helm template coder deploy/coder --namespace "$namespace" \
  --set image.repository="$image" --set image.tag="$tag" \
  --set runPods.enabled=true \
  --set runPods.image.repository="$image" --set runPods.image.tag="$tag" --set runPods.image.digest= \
  --set runPods.image.pullPolicy=IfNotPresent \
  --set workspace.placement=shared --set workspace.sharedVolume.existingClaim=work-test \
  --set runPods.quota.pods=4 \
  > "$work/all.yaml"
# Only the objects of the run pods and of the proxy (the coder itself, its database and its ExternalSecret are not under test).
python3 - "$work/all.yaml" "$work/run-pods.yaml" <<'PY'
import re, sys
wanted = {"ServiceAccount", "Role", "RoleBinding", "PriorityClass", "ResourceQuota",
          "ValidatingAdmissionPolicy", "ValidatingAdmissionPolicyBinding", "ConfigMap",
          "Deployment", "PodDisruptionBudget"}
keep = []
for doc in re.split(r"(?m)^---\s*$", open(sys.argv[1]).read()):
    kind = re.search(r"(?m)^kind: (\S+)$", doc)
    name = re.search(r"(?m)^  name: (\S+)$", doc)
    if kind and kind.group(1) in wanted:
        keep.append(doc)
    elif kind and kind.group(1) in ("NetworkPolicy", "Service") and name and re.search(r"(run-pods|-egress)$", name.group(1)):
        keep.append(doc)
open(sys.argv[2], "w").write("---\n".join(keep))
PY
kubectl apply -n "$namespace" -f "$work/run-pods.yaml"
kubectl rollout status "deployment/$proxy" -n "$namespace" --timeout=240s \
  || die "the egress proxy did not become ready (its probes are on the node's path to the pod, which a NetworkPolicy must not cut)"

say "the volume's content for the isolation check: two runs of one mirror, the second's metadata, another mirror"
# As the coder would have left it: a bare mirror with a worktree for each run (git picks the directory of each in the
# mirror's worktrees/), a file only the second run has, its metadata, and a mirror of another repository.
# shellcheck disable=SC2016  # the $mirror of the script is the pod's to expand
seed_script='set -eu
export HOME=/tmp GIT_CONFIG_GLOBAL=/dev/null GIT_AUTHOR_NAME=seed GIT_AUTHOR_EMAIL=seed@example.test GIT_COMMITTER_NAME=seed GIT_COMMITTER_EMAIL=seed@example.test
mirror=/work/git/example.test/o/r.git
mkdir -p /work/git/example.test/o /work/git/example.test/p /work/workspaces/kind-iso-a /work/workspaces/kind-iso-b /work/meta/kind-iso-b
git init -q --bare -b main "$mirror"
git init -q --bare -b main /work/git/example.test/p/other.git
git init -q -b main /tmp/seed
cd /tmp/seed
echo hi > README.md
git add README.md
git commit -qm seed
git push -q "$mirror" main
cd "$mirror"
git branch --no-track agent/kind-iso-a main
git branch --no-track agent/kind-iso-b main
git worktree add -q /work/workspaces/kind-iso-a/r agent/kind-iso-a
git worktree add -q /work/workspaces/kind-iso-b/r agent/kind-iso-b
echo private > /work/workspaces/kind-iso-b/r/private.txt
echo "{}" > /work/meta/kind-iso-b/r.json
sed -n "s/^gitdir: //p" /work/workspaces/kind-iso-a/r/.git'
kubectl delete pod seed -n "$namespace" --ignore-not-found --wait=true
kubectl apply -n "$namespace" -f - <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: seed
spec:
  terminationGracePeriodSeconds: 1
  automountServiceAccountToken: false
  securityContext: {runAsUser: 10001, runAsGroup: 10001, runAsNonRoot: true, fsGroup: 10001}
  containers:
    - name: seed
      image: $image:$tag
      imagePullPolicy: IfNotPresent
      command: [sleep, "600"]
      volumeMounts:
        - {name: work, mountPath: /work}
  volumes:
    - name: work
      persistentVolumeClaim: {claimName: work-test}
YAML
kubectl wait pod/seed -n "$namespace" --for=condition=Ready --timeout=180s \
  || { kubectl describe pod seed -n "$namespace" | tail -n 40 || true; exit 1; }
# The last line the seed prints is where git put the first run's directory in the mirror.
kubectl exec -n "$namespace" seed -- sh -c "$seed_script" > "$work/seed.out" \
  || { cat "$work/seed.out" >&2; exit 1; }
isolation_admin=$(tail -n 1 "$work/seed.out")
kubectl delete pod seed -n "$namespace" --wait=true
say "the first run's worktree directory in the mirror is $isolation_admin"
case "$isolation_admin" in /work/git/example.test/o/r.git/worktrees/?*) ;; *) echo "unexpected: $isolation_admin" >&2; exit 1 ;; esac

say "the admission policy is in force"
i=0
while [ "$i" -lt 30 ]; do
  warnings=$(kubectl get validatingadmissionpolicy -o jsonpath='{.items[*].status.typeChecking.expressionWarnings}')
  [ -z "$warnings" ] && break
  i=$((i + 1))
  sleep 1
done
# A CEL expression the API server could not type-check is a warning on the policy: show it, it is a mistake of the chart.
[ -z "$warnings" ] || { echo "the policy has type-checking warnings: $warnings" >&2; exit 1; }
sleep 3

say "a kubeconfig for the coder's ServiceAccount (the policy is matched to it)"
token=$(kubectl create token coder -n "$namespace" --duration=1h)
server=$(kubectl config view --minify -o jsonpath='{.clusters[0].cluster.server}')
ca=$(kubectl config view --raw --minify -o jsonpath='{.clusters[0].cluster.certificate-authority-data}')
cat > "$work/kubeconfig" <<YAML
apiVersion: v1
kind: Config
clusters:
  - name: test
    cluster:
      server: $server
      certificate-authority-data: $ca
users:
  - name: coder
    user:
      token: $token
contexts:
  - name: test
    context: {cluster: test, user: coder, namespace: $namespace}
current-context: test
YAML
kubectl get configmap coder-run-pod -n "$namespace" -o jsonpath='{.data.pod\.yaml}' > "$work/pod.yaml"

say "the cluster test, as the coder's ServiceAccount"
status=0
ADAM_TEST_KUBECONFIG="$work/kubeconfig" \
ADAM_TEST_KUBE_TEMPLATE="$work/pod.yaml" \
ADAM_TEST_KUBE_NAMESPACE="$namespace" \
ADAM_TEST_KUBE_INSTANCE=coder \
ADAM_TEST_KUBE_WORKER=ci-0 \
ADAM_TEST_KUBE_QUOTA_PODS=4 \
ADAM_TEST_KUBE_MODEL_KEY="$secret_key" \
ADAM_TEST_KUBE_ISOLATION_MIRROR=/work/git/example.test/o/r.git \
ADAM_TEST_KUBE_ISOLATION_ADMIN="$isolation_admin" \
ADAM_TEST_REQUIRE_KUBERNETES=1 \
  cargo test --locked -p adam-env-kubernetes --test cluster -- --nocapture || status=$?

if [ "$status" -ne 0 ]; then
  say "what the cluster says (the cluster test failed)"
  diagnostics
fi

# ---------------------------------------------------------------------------------------------------------------------
# The network policy of run pods and the egress proxy (ADR 0042), asked of a run pod. The cluster test above has released its
# pods; this one makes its own, so the quota (4 pods) has room. A failure here stops the script with the cluster's words.
# ---------------------------------------------------------------------------------------------------------------------
say "a run pod made from the chart's template, as the coder's ServiceAccount (the admission policy applies), and two plain pods"
# The coder fills in the name and the labels of a run pod (ADR 0019); so does this, from the template as the chart wrote it.
python3 - "$work/pod.yaml" "$work/run-pod.yaml" <<'PY'
import sys
template = open(sys.argv[1]).read()
head = "metadata:\n  labels:\n"
if template.count(head) != 1:
    sys.exit("the pod template has no `metadata:` with `labels:` first: " + template[:200])
labelled = template.replace(head, (
    "metadata:\n  name: egress-check\n  labels:\n"
    "    app.kubernetes.io/managed-by: adam-coder\n"
    "    app.kubernetes.io/instance: coder\n"
    "    adam.vymalo.com/run: egress-check\n"))
open(sys.argv[2], "w").write(labelled)
PY
kubectl delete pod egress-check egress-target egress-peer -n "$namespace" --ignore-not-found --wait=true
kubectl --kubeconfig "$work/kubeconfig" create -n "$namespace" -f "$work/run-pod.yaml" \
  || die "the admission policy or the quota refused the run pod made from the chart's template"
# A listener that no policy selects, and a client that no policy selects: the control that the pod network works. The
# listener is adam-egress itself on 8080 (GET /healthz answers 200 ok), which is a real server on a port nothing allows.
kubectl apply -n "$namespace" -f - <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: egress-target
  labels: {app: egress-target}
spec:
  terminationGracePeriodSeconds: 1
  automountServiceAccountToken: false
  securityContext: {runAsUser: 10001, runAsGroup: 10001, runAsNonRoot: true}
  containers:
    - name: target
      image: $image:$tag
      imagePullPolicy: IfNotPresent
      command: [tini, --, adam-egress]
      env:
        - {name: EGRESS_LISTEN, value: "0.0.0.0:8080"}
        - {name: EGRESS_ALLOW, value: registry.npmjs.org}
      readinessProbe:
        httpGet: {path: /healthz, port: 8080}
---
apiVersion: v1
kind: Pod
metadata:
  name: egress-peer
  labels: {app: egress-peer}
spec:
  terminationGracePeriodSeconds: 1
  automountServiceAccountToken: false
  securityContext: {runAsUser: 10001, runAsGroup: 10001, runAsNonRoot: true}
  containers:
    - name: peer
      image: $image:$tag
      imagePullPolicy: IfNotPresent
      command: [tini, --, sleep, infinity]
YAML
for pod in egress-check egress-target egress-peer; do
  kubectl wait "pod/$pod" -n "$namespace" --for=condition=Ready --timeout=240s || die "$pod did not become ready"
done
run_pod=egress-check
target_ip=$(kubectl get pod egress-target -n "$namespace" -o jsonpath='{.status.podIP}')
proxy_pod=$(kubectl get pod -n "$namespace" -l app.kubernetes.io/component=egress-proxy -o jsonpath='{.items[0].metadata.name}')
if [ -z "$target_ip" ] || [ -z "$proxy_pod" ]; then die "no IP for the target pod ($target_ip) or no proxy pod ($proxy_pod)"; fi

# curl_in <pod> <container> <curl arguments...>: runs curl in a pod and prints what it printed, then `exit=<status>` on a line.
curl_in() {
  pod=$1; container=$2; shift 2
  # shellcheck disable=SC2016  # the pod's shell expands these
  kubectl exec -n "$namespace" "$pod" -c "$container" -- sh -c 'curl "$@" 2>&1; echo "exit=$?"' curl "$@"
}
# refused <pod> <container> <curl arguments...>: succeeds when the connection was dropped (curl 28, the timeout) or refused
# (curl 7), and says what happened otherwise. Exit 6 (the name did not resolve) proves nothing about the network and is not
# "refused". Where nothing listens, an RST (curl 7) is what a pod with no policy answers, so with only_timeout=1 only a drop counts.
only_timeout=
refused() {
  r_pod=$1; r_container=$2; shift 2
  r_out=$(curl_in "$r_pod" "$r_container" -sS -m 6 -o /dev/null -w 'http=%{http_code}\n' "$@") || { printf 'kubectl exec failed: %s\n' "$r_out"; return 1; }
  case $r_out in
    *exit=28*) return 0 ;;
    *exit=7*)
      [ -z "$only_timeout" ] && return 0
      printf 'the connection was reset where nothing listens, which a policy that drops packets would not do: %s\n' "$r_out"
      return 1
      ;;
    *exit=6*) printf 'the name did not resolve, which says nothing about the network: %s\n' "$r_out"; return 1 ;;
    *) printf 'the connection was not refused: %s\n' "$r_out"; return 1 ;;
  esac
}
# must_be_refused <description> <pod> <container> <curl arguments...>: refused, retried for up to 60 s (kindnet programs a policy
# a moment after it sees a pod). With ADAM_TEST_CNI_MAY_IGNORE_POLICIES=1 a connection that is not refused is a warning.
must_be_refused() {
  m_what=$1; shift
  if [ "$cni_may_ignore" = 1 ]; then
    if m_last=$(refused "$@"); then
      ok "$m_what"
    else
      printf '%s\n' "::warning::$m_what: NOT refused ($(printf '%s' "$m_last" | tail -c 200)). ADAM_TEST_CNI_MAY_IGNORE_POLICIES=1 says this cluster does not enforce NetworkPolicies, so the policies are installed and valid and their enforcement is not proven here"
    fi
  else
    until_ok 60 "$m_what (the network policy to be enforced)" refused "$@"
    ok "$m_what"
  fi
}
# in_run_pod <command...>: a command in the run pod's container.
in_run_pod() { kubectl exec -n "$namespace" "$run_pod" -c run -- "$@"; }

say "the run pod's environment is the template's: every command it runs reaches the internet through the proxy, or not at all"
for v in HTTPS_PROXY https_proxy HTTP_PROXY http_proxy; do
  got=$(in_run_pod printenv "$v") || die "$v is not set in the run pod"
  [ "$got" = "$proxy_url" ] || die "$v in the run pod is '$got', not '$proxy_url'"
done
for v in NO_PROXY no_proxy; do
  got=$(in_run_pod printenv "$v") || die "$v is not set in the run pod"
  [ "$got" = "localhost,127.0.0.1" ] || die "$v in the run pod is '$got', not 'localhost,127.0.0.1'"
done
ok "HTTPS_PROXY, https_proxy, HTTP_PROXY and http_proxy are $proxy_url, NO_PROXY and no_proxy are localhost,127.0.0.1"

say "DNS works from the run pod (the policy allows it), for the cluster's names and the internet's"
api_name=kubernetes.default.svc
in_run_pod getent hosts "$api_name" > "$work/dns-api.out" || die "the run pod cannot resolve $api_name: DNS is cut off, so the refusals below would prove nothing"
in_run_pod getent hosts registry.npmjs.org > "$work/dns-npm.out" || die "the run pod cannot resolve registry.npmjs.org: DNS is cut off, so the refusals below would prove nothing"
in_run_pod getent hosts "$proxy.$namespace.svc" > "$work/dns-proxy.out" || die "the run pod cannot resolve the proxy's Service name"
api_ip=$(awk 'NR == 1 { print $1 }' "$work/dns-api.out")
ok "the run pod resolves $api_name ($api_ip), registry.npmjs.org and $proxy.$namespace.svc"

say "the controls: the network works, so a refusal below is the policy and nothing else"
# A pod that no policy selects reaches the target: the pod network is open, and a refusal of the run pod is the policy's.
peer_out=$(curl_in egress-peer peer -sS -m 10 -o /dev/null -w 'http=%{http_code}\n' "http://$target_ip:8080/healthz") || die "kubectl exec in the peer pod failed: $peer_out"
case $peer_out in *http=200*exit=0*) ;; *) die "a pod no policy selects does not reach the target pod at $target_ip:8080: $peer_out" ;; esac
ok "a pod no policy selects reaches another pod ($target_ip:8080): the pod network works"
# The proxy's own pod reaches a public address on 443 (its policy allows it): the cluster reaches the internet, and a
# refusal of the run pod is not an offline runner. Retried: the pod's policy is programmed a moment after it starts.
proxy_reaches() {
  p_out=$(curl_in "$proxy_pod" egress -sS -m 15 -o /dev/null -w 'http=%{http_code}\n' https://registry.npmjs.org/) || { printf '%s\n' "$p_out"; return 1; }
  case $p_out in *http=200*exit=0*) ;; *) printf '%s\n' "$p_out"; return 1 ;; esac
}
until_ok 60 "the proxy's own pod to reach https://registry.npmjs.org/ (the control: this cluster reaches the internet, and the proxy's policy allows 443)" proxy_reaches
ok "the proxy's own pod reaches https://registry.npmjs.org/ on 443: the cluster has a way to the internet"

say "refused: a run pod that goes around the proxy"
must_be_refused "a direct connection from the run pod to a public address it resolves (curl --noproxy '*' https://registry.npmjs.org/) is refused" \
  "$run_pod" run --noproxy '*' https://registry.npmjs.org/
must_be_refused "a direct connection from the run pod to the API server by its Service name, which DNS resolves ($api_name), is refused" \
  "$run_pod" run --noproxy '*' -k "https://$api_name/version"
api_endpoint=$(kubectl get endpoints kubernetes -o jsonpath='{.subsets[0].addresses[0].ip}:{.subsets[0].ports[0].port}')
[ -n "$api_endpoint" ] || die "no address of the API server in the endpoints of the kubernetes Service"
must_be_refused "... and by its address ($api_endpoint) is refused" \
  "$run_pod" run --noproxy '*' -k "https://$api_endpoint/version"
must_be_refused "a direct connection from the run pod to another pod's IP ($target_ip:8080, which the control reached) is refused" \
  "$run_pod" run --noproxy '*' "http://$target_ip:8080/healthz"
proxy_ip=$(kubectl get pod "$proxy_pod" -n "$namespace" -o jsonpath='{.status.podIP}')
only_timeout=1
must_be_refused "a run pod reaches the proxy's pod on its port only: a packet to another port ($proxy_ip:8080) is dropped" \
  "$run_pod" run --noproxy '*' "http://$proxy_ip:8080/healthz"
only_timeout=
must_be_refused "a pod that is not a run pod cannot use the proxy (its policy admits the run pods alone)" \
  egress-peer peer -x "$proxy_url" https://registry.npmjs.org/
must_be_refused "the proxy's own pod reaches no public address on 80 (its policy allows 443 only)" \
  "$proxy_pod" egress http://registry.npmjs.org/
must_be_refused "... and no pod of the cluster ($target_ip:8080)" \
  "$proxy_pod" egress "http://$target_ip:8080/healthz"

say "allowed: what the run pod does through the proxy, with the environment the template gives it"
through_proxy() {
  t_out=$(curl_in "$run_pod" run -sS -m 30 -o /dev/null -w 'http=%{http_code}\n' https://registry.npmjs.org/) || { printf '%s\n' "$t_out"; return 1; }
  case $t_out in *http=200*exit=0*) ;; *) printf '%s\n' "$t_out"; return 1 ;; esac
}
until_ok 90 "curl in the run pod to reach https://registry.npmjs.org/ through the proxy" through_proxy
ok "curl https://registry.npmjs.org/ in the run pod answers 200 through the proxy"
npm_view() {
  n_out=$(in_run_pod env HOME=/tmp npm_config_update_notifier=false npm view is-number version 2>&1) || { printf '%s\n' "$n_out"; return 1; }
  printf '%s\n' "$n_out" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+' || { printf 'not a version: %s\n' "$n_out"; return 1; }
}
until_ok 120 "npm view is-number version in the run pod to succeed through the proxy" npm_view
ok "npm view is-number version in the run pod succeeds through the proxy"

say "refused by the proxy: a host that is not allowed gets 403, and the log has the decision and nothing of the request"
not_allowed=$(curl_in "$run_pod" run -sS -m 20 -o /dev/null -w 'http=%{http_code} connect=%{http_connect}\n' https://example.com/) \
  || die "kubectl exec failed: $not_allowed"
case $not_allowed in
  *connect=403*exit=56*) ;;
  *) die "https://example.com/ through the proxy was not refused with 403 on the CONNECT (curl 56): $not_allowed" ;;
esac
ok "curl https://example.com/ through the proxy: the CONNECT is answered 403 (curl exit 56)"
# A request that carries what must never reach a log: a Proxy-Authorization header and a query. Plain HTTP to the proxy, as an
# absolute URL (the proxy answers 403, only HTTPS through CONNECT), and a CONNECT with the header (--proxy-header).
plain=$(curl_in "$run_pod" run -sS -m 15 -o /dev/null -w 'http=%{http_code}\n' -x "$proxy_url" \
  -H 'Proxy-Authorization: Basic c2VjcmV0' 'http://example.com/private/path?token=secret') || die "kubectl exec failed: $plain"
case $plain in *http=403*exit=0*) ;; *) die "plain HTTP to the proxy was not answered 403: $plain" ;; esac
header=$(curl_in "$run_pod" run -sS -m 15 -o /dev/null -w 'http=%{http_code} connect=%{http_connect}\n' \
  --proxy-header 'Proxy-Authorization: Basic c2VjcmV0' 'https://example.com/private/path?token=secret') || die "kubectl exec failed: $header"
case $header in *connect=403*exit=56*) ;; *) die "a CONNECT with a Proxy-Authorization header was not refused 403: $header" ;; esac
ok "plain HTTP (an absolute URL, a header, a query) is answered 403, and so is a CONNECT that carries the header"
proxy_log() { kubectl logs -n "$namespace" -l app.kubernetes.io/component=egress-proxy --all-containers --prefix --tail=-1; }
log_has_refusals() {
  proxy_log > "$work/proxy.log" 2>&1 || { cat "$work/proxy.log"; return 1; }
  grep -F '"decision":"refuse"' "$work/proxy.log" | grep -Fq '"host":"example.com"' || { echo 'no refusal of example.com yet'; return 1; }
  grep -F '"decision":"refuse"' "$work/proxy.log" | grep -Fq '"reason":"host not allowed"' || { echo 'no "host not allowed" yet'; return 1; }
  grep -F '"decision":"refuse"' "$work/proxy.log" | grep -Fq '"reason":"plain http request"' || { echo 'no "plain http request" yet'; return 1; }
}
until_ok 30 "the proxy's log to hold the refusals (decision=refuse, the host, the reason)" log_has_refusals
ok "the proxy's log has decision=refuse for example.com (host not allowed) and for the plain HTTP request"
grep -F '"decision":"allow"' "$work/proxy.log" | grep -Fq '"host":"registry.npmjs.org"' \
  || die "the proxy's log has no allow line for registry.npmjs.org: $(grep -F '"decision"' "$work/proxy.log" | tail -n 5)"
ok "... and decision=allow for registry.npmjs.org"
for leak in secret c2VjcmV0 token private/path Proxy-Authorization; do
  if grep -Fq -- "$leak" "$work/proxy.log"; then
    die "the proxy's log holds '$leak', which only the request carried: $(grep -F -- "$leak" "$work/proxy.log" | head -n 3)"
  fi
done
ok "no log line holds the credential, the query or the path that the refused requests carried"

say "IPv6: no rule of any policy names an IPv6 range (kind is IPv4 only, so this is the proof available: IPv6 egress is denied by the absence of a rule)"
kubectl get networkpolicy -n "$namespace" -o json | python3 -c '
import json, sys
cidrs = []
for policy in json.load(sys.stdin)["items"]:
    for rule in policy["spec"].get("egress", []) + policy["spec"].get("ingress", []):
        for peer in rule.get("to", []) + rule.get("from", []):
            block = peer.get("ipBlock")
            if block:
                cidrs += [block["cidr"]] + block.get("except", [])
v6 = [c for c in cidrs if ":" in c]
if v6:
    sys.exit("an IPv6 range in a policy: %s" % v6)
if not cidrs:
    sys.exit("no ipBlock at all: the check would prove nothing")
print("%d ranges, none IPv6" % len(cidrs))
' || die "the policies name an IPv6 range, or none"
ok "the applied policies' ranges are IPv4 only"

say "clean up what the network checks made"
kubectl delete pod egress-check egress-target egress-peer -n "$namespace" --wait=false || true

exit "$status"
