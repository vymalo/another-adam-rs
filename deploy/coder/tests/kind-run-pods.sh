#!/bin/sh
# Run pods against a real cluster (ADR 0019): applies the chart's run-pod objects to the cluster kubectl points at
# (a kind cluster in CI) and runs the cluster test of adam-env-kubernetes against it **as the coder's ServiceAccount**,
# so that the chart's admission policy applies to what the test makes.
#
#   kind create cluster
#   sh deploy/coder/tests/kind-run-pods.sh        (from the repository root)
#
# Needs docker, kubectl, helm, cargo and python3 (to read a JSON field), and a cluster of Kubernetes 1.30 or later (the
# script stops on an older one). What it proves, and what it does not, is in deploy/coder/README.md ("Run pods"):
#
#   * the objects the chart renders for runPods apply, the quota and the admission policy take effect, and a run pod
#     made from the chart's own template runs commands, is killed, swept when idle and released (the cluster test);
#   * not that the coder runs a task over a model through it, and not the NetworkPolicy (kind's network plugin may not
#     enforce one).
#
# The images are one small local image (docker/coder/test/run-pod.Dockerfile) that plays both the run container and
# the coder's image for the init container, loaded into kind. NAMESPACE and KIND_NAME can be set.
set -eu

namespace=${NAMESPACE:-coder-ns}
kind_name=${KIND_NAME:-kind}
image=adam-run-test
tag=ci
secret_key=ci-model-key
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

say() { printf '== %s\n' "$*"; }

minor=$(kubectl version -o json | python3 -c 'import json,sys; print(json.load(sys.stdin)["serverVersion"]["minor"].rstrip("+"))')
say "the cluster is Kubernetes 1.$minor"
if [ "$minor" -lt 30 ]; then
  echo "the admission policy needs Kubernetes 1.30 or later" >&2
  exit 1
fi

say "the image of the run pods, loaded into kind"
docker build -q -f docker/coder/test/run-pod.Dockerfile -t "$image:$tag" .
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

say "the chart's run-pod objects"
helm template coder deploy/coder --namespace "$namespace" \
  --set image.repository="$image" --set image.tag="$tag" \
  --set runPods.enabled=true \
  --set runPods.image.repository="$image" --set runPods.image.tag="$tag" --set runPods.image.digest= \
  --set runPods.image.pullPolicy=IfNotPresent \
  --set workspace.placement=shared --set workspace.sharedVolume.existingClaim=work-test \
  --set runPods.quota.pods=4 \
  > "$work/all.yaml"
# Only the objects of the run pods (the coder itself, its database and its ExternalSecret are not under test).
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
    elif kind and kind.group(1) == "NetworkPolicy" and name and name.group(1).endswith("run-pods"):
        keep.append(doc)
open(sys.argv[2], "w").write("---\n".join(keep))
PY
kubectl apply -n "$namespace" -f "$work/run-pods.yaml"

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
ADAM_TEST_REQUIRE_KUBERNETES=1 \
  cargo test --locked -p adam-env-kubernetes --test cluster -- --nocapture || status=$?

if [ "$status" -ne 0 ]; then
  say "what the cluster says (the test failed)"
  kubectl get pods,events -n "$namespace" -o wide || true
  kubectl describe pods -n "$namespace" | tail -n 120 || true
  kubectl get validatingadmissionpolicy -o yaml | tail -n 40 || true
fi
exit "$status"
