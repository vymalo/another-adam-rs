#!/bin/sh
# No shared writable cache (ADR 0040, amended 2026-11-26; ADR 0041, decision 2): two people's runs cannot share a
# poisoned file. The chart mounts no cache, so /cache is a directory of each run container's own file system; a run
# pod may mount a cache claim only when the deployment names it in cache.allowedClaims, and the admission policy
# refuses any other claim.
#
# What it proves, on a real cluster:
#   * the rendered run pod template carries no cache volume and no mount at /cache (red on the chart of before this
#     change, whose template mounted the shared claim when cache.create or cache.existingClaim was set);
#   * run A writes a file under /cache and run B, another admitted run pod, does not see it, and its npm cache
#     (/cache/npm) is its own;
#   * the control: two pods that both mount the same claim at /cache DO share the file, so the check above can fail;
#   * the admission policy admits a run pod that mounts the work claim and a claim named in cache.allowedClaims, and
#     refuses one that mounts a claim it does not name.
#
# Prerequisites, as for deploy/coder/tests/kind-run-pods.sh: a kind cluster of Kubernetes 1.37 or later that kubectl
# points at, and the image that script builds and loads (adam-run-test:ci). Run this after it, on the same cluster:
#
#   kind create cluster --image kindest/node:v1.37.0@sha256:a1ed56cfb0e7b93589bdf97c8cd566405a265939e3620fc4f5de89adff580ae5
#   sh deploy/coder/tests/kind-run-pods.sh
#   sh deploy/coder/tests/kind-cache-isolation.sh        (from the repository root)
#
# NAMESPACE, KIND_NAME, IMAGE and TAG can be set. Needs kubectl, helm, python3 and docker (to check the image is
# loaded).
set -eu

namespace=${NAMESPACE:-coder-ns}
image=${IMAGE:-adam-run-test}
tag=${TAG:-ci}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

say() { printf '== %s\n' "$*"; }
ok() { printf 'ok   %s\n' "$*"; }
die() {
  printf 'FAIL %s\n' "$*" >&2
  kubectl get pods -n "$namespace" -o wide || true
  kubectl get events -n "$namespace" --sort-by=.lastTimestamp | tail -n 30 || true
  exit 1
}

say "the image kind-run-pods.sh builds and loads (adam-run-test:ci unless IMAGE/TAG say otherwise)"
docker image inspect "$image:$tag" >/dev/null 2>&1 \
  || die "the image $image:$tag is not built: run deploy/coder/tests/kind-run-pods.sh first (it builds and loads it)"

minor=$(kubectl version -o json | python3 -c 'import json,sys; print(json.load(sys.stdin)["serverVersion"]["minor"].rstrip("+"))')
if [ "$minor" -lt 37 ]; then
  echo "the chart's floor is Kubernetes 1.37 (ADR 0080)" >&2
  exit 1
fi

say "the namespace, the work claim, and two claims a deployment might admit as a cache"
kubectl create namespace "$namespace" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
kubectl apply -n "$namespace" -f - >/dev/null <<YAML
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: work-test
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 1Gi
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: user-cache
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 1Gi
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: other-cache
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 1Gi
YAML

say "the chart's run-pod objects, with cache.allowedClaims={user-cache}"
# Only the objects of the run pods and of the proxy, as kind-run-pods.sh does: the coder itself, its database and its
# ExternalSecret are not under test.
helm template coder deploy/coder --namespace "$namespace" \
  --set image.repository="$image" --set image.tag="$tag" \
  --set runPods.enabled=true \
  --set runPods.image.repository="$image" --set runPods.image.tag="$tag" --set runPods.image.digest= \
  --set runPods.image.pullPolicy=IfNotPresent \
  --set "runPods.repositoryImages.allowedPrefixes={$image:}" \
  --set workspace.placement=shared --set workspace.sharedVolume.existingClaim=work-test \
  --set 'cache.allowedClaims={user-cache}' \
  --set runPods.quota.pods=4 \
  > "$work/all.yaml"
python3 - "$work/all.yaml" "$work/run-pods.yaml" <<'PY'
import re, sys
wanted = {"ServiceAccount", "Role", "RoleBinding", "PriorityClass", "ResourceQuota",
          "ValidatingAdmissionPolicy", "ValidatingAdmissionPolicyBinding", "ConfigMap"}
keep = []
for doc in re.split(r"(?m)^---\s*$", open(sys.argv[1]).read()):
    kind = re.search(r"(?m)^kind: (\S+)$", doc)
    name = re.search(r"(?m)^  name: (\S+)$", doc)
    if kind and kind.group(1) in wanted:
        keep.append(doc)
    elif kind and kind.group(1) == "NetworkPolicy" and name and name.group(1).endswith("-run-pods"):
        keep.append(doc)
open(sys.argv[2], "w").write("---\n".join(keep))
PY
kubectl apply -n "$namespace" -f "$work/run-pods.yaml" >/dev/null

say "the admission policy is in force"
i=0
while [ "$i" -lt 30 ]; do
  warnings=$(kubectl get validatingadmissionpolicy -o jsonpath='{.items[*].status.typeChecking.expressionWarnings}')
  [ -z "$warnings" ] && break
  i=$((i + 1))
  sleep 1
done
# A CEL expression the API server could not type-check is a warning on the policy: show it, it is a mistake of the chart.
[ -z "$warnings" ] || die "the policy has type-checking warnings: $warnings"

say "the rendered template and policy carry no shared cache"
kubectl get configmap coder-run-pod -n "$namespace" -o jsonpath='{.data.pod\.yaml}' > "$work/pod.yaml"
# Red on the chart before this change: cache.create or cache.existingClaim put a cache volume and a /cache mount in
# this template.
grep -Eq 'mountPath: /cache$' "$work/pod.yaml" && die "the run pod template mounts /cache: the chart still provides a shared cache"
grep -Eq 'name: cache$' "$work/pod.yaml" && die "the run pod template has a cache volume: the chart still provides a shared cache"
ok "the run pod template mounts no /cache and has no cache volume"
grep -Fq "claimName in ['work-test', 'user-cache']" "$work/run-pods.yaml" \
  || die "the admission policy does not admit the work claim and cache.allowedClaims"
ok "the admission policy admits the work claim and the one claim cache.allowedClaims names"

say "a kubeconfig for the coder's ServiceAccount (the admission policy is matched to it)"
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

# run_pod <name> <run> <file>: the chart's template, with the name and labels the coder fills in (ADR 0019).
run_pod() {
  python3 - "$work/pod.yaml" "$3" "$1" "$2" <<'PY'
import sys
template, out, name, run = sys.argv[1:5]
text = open(template).read()
head = "metadata:\n  labels:\n"
if text.count(head) != 1:
    sys.exit("the pod template has no `metadata:` with `labels:` first: " + text[:200])
text = text.replace(head, (
    "metadata:\n  name: %s\n  labels:\n"
    "    app.kubernetes.io/managed-by: adam-coder\n"
    "    app.kubernetes.io/instance: coder\n"
    "    adam.vymalo.com/run: %s\n" % (name, run)))
open(out, "w").write(text)
PY
}

say "two run pods made from the chart's template, as the coder's ServiceAccount"
kubectl delete pod cache-a cache-b -n "$namespace" --ignore-not-found --wait=true >/dev/null
run_pod cache-a cache-a "$work/cache-a.yaml"
run_pod cache-b cache-b "$work/cache-b.yaml"
kubectl --kubeconfig "$work/kubeconfig" create -n "$namespace" -f "$work/cache-a.yaml" >/dev/null \
  || die "the admission policy refused run A made from the chart's template"
kubectl --kubeconfig "$work/kubeconfig" create -n "$namespace" -f "$work/cache-b.yaml" >/dev/null \
  || die "the admission policy refused run B made from the chart's template"
kubectl wait pod/cache-a pod/cache-b -n "$namespace" --for=condition=Ready --timeout=180s >/dev/null \
  || die "the two run pods did not become ready"

say "run A writes a file into the cache path, as a poisoned build would"
kubectl exec -n "$namespace" cache-a -c run -- sh -c \
  'mkdir -p /cache/npm/_cacache /cache/cargo && printf poison > /cache/npm/_cacache/poison && printf "[build]\nrustc-wrapper = /work/evil\n" > /cache/cargo/config.toml' \
  || die "run A could not write under /cache"
kubectl exec -n "$namespace" cache-a -c run -- sh -c 'test -f /cache/cargo/config.toml && test -f /cache/npm/_cacache/poison' \
  || die "run A's own /cache does not hold what it wrote"
ok "run A's own /cache holds the file it wrote"

say "run B's build never reads it"
kubectl exec -n "$namespace" cache-b -c run -- sh -c \
  'if [ -e /cache/cargo/config.toml ] || [ -e /cache/npm/_cacache/poison ]; then echo POISON-VISIBLE; exit 1; fi' \
  || die "run B sees the file run A wrote under /cache: the runs share a cache"
ok "run B does not see run A's file under /cache"
kubectl exec -n "$namespace" cache-b -c run -- sh -c \
  'mkdir -p /cache/npm && HOME=/tmp npm_config_cache=/cache/npm npm cache verify >/dev/null 2>&1' \
  || die "run B's npm could not read its own cache at /cache/npm"
ok "run B's npm cache (/cache/npm) is its own and verifies"
kubectl delete pod cache-a cache-b -n "$namespace" --wait=true >/dev/null

say "the control: two pods that both mount one claim at /cache do share the file (so the check above can fail)"
# The shape the chart used to render: a cache claim mounted at /cache by every run. The control is made by kubectl
# (not the coder's ServiceAccount), because the admission policy is what refuses this shape for a run pod.
kubectl delete pod shared-a shared-b -n "$namespace" --ignore-not-found --wait=true >/dev/null
for p in shared-a shared-b; do
  kubectl apply -n "$namespace" -f - >/dev/null <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: $p
spec:
  terminationGracePeriodSeconds: 1
  automountServiceAccountToken: false
  securityContext: {runAsUser: 10001, runAsGroup: 10001, runAsNonRoot: true, fsGroup: 10001}
  containers:
    - name: run
      image: $image:$tag
      imagePullPolicy: IfNotPresent
      command: [tini, --, sleep, "600"]
      volumeMounts:
        - {name: cache, mountPath: /cache}
  volumes:
    - name: cache
      persistentVolumeClaim: {claimName: user-cache}
YAML
done
kubectl wait pod/shared-a pod/shared-b -n "$namespace" --for=condition=Ready --timeout=180s >/dev/null \
  || die "the two control pods did not become ready"
kubectl exec -n "$namespace" shared-a -c run -- sh -c 'mkdir -p /cache && printf shared > /cache/control.txt' \
  || die "the control's first pod could not write to the shared claim"
kubectl exec -n "$namespace" shared-b -c run -- sh -c 'test -f /cache/control.txt' \
  || die "the control did not share the file: this check proves nothing"
ok "the control: a shared cache claim does carry one pod's file to the other"
kubectl delete pod shared-a shared-b -n "$namespace" --wait=true >/dev/null

say "the admission policy: cache.allowedClaims is the whole list"
# A run pod from the chart's template plus one cache claim, asked of the API server with a server-side dry run as the
# coder's ServiceAccount. The control is the same pod without the extra volume, which the chart's template already
# passes above.
with_claim() { # with_claim <claim> <file>
  python3 - "$work/cache-check.yaml" "$2" "$1" <<'PY'
import sys
template, out, claim = sys.argv[1:4]
text = open(template).read()
mount = "        - name: tools\n          mountPath: /opt/adam/bin\n"
if text.count(mount) != 1:
    sys.exit("the run container's tools mount is not where the template says: " + text[:400])
text = text.replace(mount, (
    "        - name: cache\n"
    "          mountPath: /cache\n" + mount))
tools = "    - name: tools\n      emptyDir: {}\n"
if text.count(tools) != 1:
    sys.exit("the template's tools volume is not where it says: " + text[-400:])
text = text.replace(tools, tools + (
    "    - name: cache\n"
    "      persistentVolumeClaim:\n"
    "        claimName: %s\n" % claim))
open(out, "w").write(text)
PY
}
run_pod cache-check cache-check "$work/cache-check.yaml"
with_claim user-cache "$work/admitted.yaml"
kubectl --kubeconfig "$work/kubeconfig" create -n "$namespace" --dry-run=server -f "$work/admitted.yaml" >/dev/null \
  || die "the admission policy refused a claim cache.allowedClaims names (user-cache)"
ok "admitted: a run pod that mounts a claim cache.allowedClaims names"
with_claim other-cache "$work/refused.yaml"
if kubectl --kubeconfig "$work/kubeconfig" create -n "$namespace" --dry-run=server -f "$work/refused.yaml" >/dev/null 2>&1; then
  die "the admission policy admitted a claim cache.allowedClaims does not name (other-cache)"
fi
ok "refused: a run pod that mounts a claim cache.allowedClaims does not name"

say "the cache isolation check passed"
