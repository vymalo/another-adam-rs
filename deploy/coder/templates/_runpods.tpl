{{/*
Run pods (values `runPods`, ADR 0019): a pod of its own for each active run. Everything here is rendered only when
runPods.enabled is true and the role runs workers (a control plane starts no commands).
*/}}

{{/* Whether run pods are on for this release: "true" or nothing. */}}
{{- define "coder.runPods" -}}
{{- if and (eq (toString .Values.runPods.enabled) "true") (include "coder.runsWorkers" .) -}}true{{- end -}}
{{- end -}}

{{/* The priority class run pods have: the one named in values, else the chart's own <release>-run. */}}
{{- define "coder.runPodsPriorityClass" -}}
{{- default (printf "%s-run" (include "coder.fullname" .) | trunc 63 | trimSuffix "-") (default "" .Values.runPods.priorityClassName | toString | trim) -}}
{{- end -}}

{{/* Whether the chart renders that PriorityClass itself: no name in values. "true" or nothing. */}}
{{- define "coder.runPodsOwnPriorityClass" -}}
{{- if not (default "" .Values.runPods.priorityClassName | toString | trim) -}}true{{- end -}}
{{- end -}}

{{/* The ServiceAccount of the coder pod while run pods are on. */}}
{{- define "coder.serviceAccountName" -}}
{{- default (include "coder.fullname" .) (default "" .Values.runPods.serviceAccount.name | toString | trim) -}}
{{- end -}}

{{/* Whether the chart renders the ServiceAccount: no name in values. "true" or nothing. */}}
{{- define "coder.ownServiceAccount" -}}
{{- if not (default "" .Values.runPods.serviceAccount.name | toString | trim) -}}true{{- end -}}
{{- end -}}

{{/* The name of the Role, RoleBinding, ResourceQuota and NetworkPolicy of the run pods. */}}
{{- define "coder.runPodsName" -}}
{{- printf "%s-run-pods" (include "coder.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/* The name of the ConfigMap that holds the pod template, and where it is mounted. */}}
{{- define "coder.runPodTemplateConfigMapName" -}}
{{- printf "%s-run-pod" (include "coder.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- define "coder.runPodTemplateDir" -}}/etc/adam/run-pod{{- end -}}
{{- define "coder.runPodTemplateFile" -}}/etc/adam/run-pod/pod.yaml{{- end -}}

{{/*
The names of the admission policy and its binding: cluster-scoped, so they carry the namespace as well as the
release.
*/}}
{{- define "coder.runPodsPolicyName" -}}
{{- printf "%s-%s-run-pods" (include "coder.fullname" .) .Release.Namespace | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/* The run container's image, as the pod spells it: repository:tag, and @digest when there is one. */}}
{{- define "coder.runPodImage" -}}
{{- $i := .Values.runPods.image -}}
{{- if $i.digest -}}{{ printf "%s:%s@%s" $i.repository (toString $i.tag) $i.digest }}{{- else -}}{{ printf "%s:%s" $i.repository (toString $i.tag) }}{{- end -}}
{{- end -}}

{{/* The init container's image: the coder's own unless runPods.toolsImage says another. */}}
{{- define "coder.runPodToolsImage" -}}
{{- $t := .Values.runPods.toolsImage -}}
{{- if and $t.repository $t.tag -}}{{ printf "%s:%s" $t.repository (toString $t.tag) }}{{- else -}}{{ printf "%s:%s" .Values.image.repository (toString .Values.image.tag) }}{{- end -}}
{{- end -}}

{{/*
The claim a run pod mounts at /work: the coder's workspace volume. The shared claim (placement shared or affinity), or
the per-pod claim of the StatefulSet's volumeClaimTemplates for worker 0 (<claim template name>-<pod name>): the chart
refuses more than one replica with it, because a run pod can mount one claim.
*/}}
{{- define "coder.runPodWorkClaim" -}}
{{- if include "coder.sharedWork" . -}}{{ include "coder.workClaim" . }}{{- else -}}{{ printf "work-%s-0" (include "coder.fullname" .) }}{{- end -}}
{{- end -}}

{{/*
The one key of the coder's Secret a run pod reads: the model key OpenCode uses (secret_ref "model-key"). The pod template
sets it and the admission policy allows exactly this reference, by the Secret's name (coder.secretName) and this key.
*/}}
{{- define "coder.runPodSecretKey" -}}MODEL_API_KEY{{- end -}}

{{/* The name label of run pods: not the coder's, so the StatefulSet's selector and the coder's NetworkPolicy never match them. */}}
{{- define "coder.runPodNameLabel" -}}
{{- printf "%s-run" (include "coder.name" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
The selector of the run pods, by the labels the coder puts on every pod it makes (ADR 0019): used by their NetworkPolicy and,
as the only ingress, by the egress proxy's. Indent it to the selector's level.
*/}}
{{- define "coder.runPodSelector" -}}
matchLabels:
  app.kubernetes.io/managed-by: adam-coder
  app.kubernetes.io/instance: {{ .Release.Name }}
matchExpressions:
  - key: adam.vymalo.com/run
    operator: Exists
{{- end -}}

{{/*
The egress proxy (values runPods.egressProxy, ADR 0042): the only way out of a run pod to the internet. Its name (the
Deployment, Service, ServiceAccount, PodDisruptionBudget and NetworkPolicy), its port, the labels of its pods (not the
coder's, not the run pods', and not the front's: no selector of another object matches them), where its allow-list is
mounted, and the URL the run pods' environment names.
*/}}
{{- define "coder.egressProxyName" -}}
{{- printf "%s-egress" (include "coder.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- define "coder.egressProxyAllowName" -}}
{{- printf "%s-egress-allow" (include "coder.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- define "coder.egressProxyPort" -}}3128{{- end -}}
{{- define "coder.egressProxyAllowDir" -}}/etc/adam-egress{{- end -}}
{{- define "coder.egressProxySelectorLabels" -}}
app.kubernetes.io/name: {{ printf "%s-egress" (include "coder.name" .) | trunc 63 | trimSuffix "-" }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: egress-proxy
{{- end -}}
{{- define "coder.egressProxyLabels" -}}
{{ include "coder.egressProxySelectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end -}}
{{- define "coder.egressProxyURL" -}}
{{ printf "http://%s.%s.svc:%s" (include "coder.egressProxyName" .) .Release.Namespace (include "coder.egressProxyPort" .) }}
{{- end -}}

{{/* The hosts the proxy allows, lower case, once each, one per line: `allow`, then `extraAllow`. Empty when there is none. */}}
{{- define "coder.egressProxyAllowList" -}}
{{- $hosts := list -}}
{{- range concat (default (list) .Values.runPods.egressProxy.allow) (default (list) .Values.runPods.egressProxy.extraAllow) -}}
{{- $hosts = append $hosts (lower (trim (toString .))) -}}
{{- end -}}
{{- join "\n" ($hosts | uniq) -}}
{{- end -}}

{{/* NO_PROXY of a run pod: the loopback names, then runPods.egressProxy.extraNoProxy. */}}
{{- define "coder.egressProxyNoProxy" -}}
{{- join "," (concat (list "localhost" "127.0.0.1") (default (list) .Values.runPods.egressProxy.extraNoProxy)) -}}
{{- end -}}

{{/* The IPv4 ranges the proxy never connects to and its policy leaves out: networkPolicy.blockedCIDRs and clusterCIDRs, once each. */}}
{{- define "coder.egressDenyCIDRs" -}}
{{- join "," (concat (default (list) .Values.runPods.networkPolicy.blockedCIDRs) (default (list) .Values.runPods.networkPolicy.clusterCIDRs) | uniq) -}}
{{- end -}}

{{/*
The pod every run pod is made from: a Pod, as YAML, with no name, namespace or owner (the coder fills in a name, the
namespace, the labels app.kubernetes.io/managed-by, app.kubernetes.io/instance and adam.vymalo.com/run, and the
annotations adam.vymalo.com/run-id, adam.vymalo.com/worker and adam.vymalo.com/workspace-mounts) and replaces the run
container's one mount of the workspace volume (the volume `work` at the coder's WORKSPACE_ROOT, whole) by the mounts of the
run's own directory and of the git directories its worktrees need (ADR 0041), so that a run pod sees no other run. The
coder refuses to start when the run container does not mount a volume at WORKSPACE_ROOT once and whole. Read by the coder at
startup (RUN_POD_TEMPLATE_FILE).
*/}}
{{- define "coder.runPodTemplate" -}}
{{- $perPod := not (include "coder.sharedWork" .) -}}
apiVersion: v1
kind: Pod
metadata:
  labels:
    app.kubernetes.io/name: {{ include "coder.runPodNameLabel" . }}
    app.kubernetes.io/component: run
spec:
  priorityClassName: {{ include "coder.runPodsPriorityClass" . }}
  # A run pod holds no credential for the cluster.
  automountServiceAccountToken: false
  enableServiceLinks: false
  terminationGracePeriodSeconds: 5
  {{- with .Values.imagePullSecrets }}
  imagePullSecrets:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  securityContext:
    {{- toYaml .Values.podSecurityContext | nindent 4 }}
  {{- if or $perPod .Values.runPods.nodeAffinity }}
  affinity:
    {{- if $perPod }}
    {{- /* The volume is a ReadWriteOnce claim of the coder pod, attached to one node: run where the coder is. */}}
    podAffinity:
      requiredDuringSchedulingIgnoredDuringExecution:
        - topologyKey: kubernetes.io/hostname
          labelSelector:
            matchLabels:
              {{- include "coder.selectorLabels" . | nindent 14 }}
    {{- end }}
    {{- with .Values.runPods.nodeAffinity }}
    nodeAffinity:
      {{- toYaml . | nindent 6 }}
    {{- end }}
  {{- end }}
  {{- with .Values.runPods.nodeSelector }}
  nodeSelector:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  {{- with .Values.runPods.tolerations }}
  tolerations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  initContainers:
    - name: tools
      image: {{ include "coder.runPodToolsImage" . | quote }}
      imagePullPolicy: {{ .Values.image.pullPolicy }}
      # adam-exec and the native OpenCode, out of the coder's image, into the directory the run container mounts
      # read-only: the layout of a devcontainer's tools directory.
      command: ["sh", "-c", "cp -L /opt/adam/bin/adam-exec /opt/adam/bin/opencode /tools/ && chmod 0755 /tools/adam-exec /tools/opencode"]
      securityContext:
        {{- toYaml .Values.containerSecurityContext | nindent 8 }}
      resources:
        requests:
          cpu: 10m
          memory: 32Mi
        limits:
          memory: 256Mi
      volumeMounts:
        - name: tools
          mountPath: /tools
  containers:
    - name: {{ .Values.runPods.container }}
      image: {{ include "coder.runPodImage" . | quote }}
      imagePullPolicy: {{ .Values.runPods.image.pullPolicy }}
      workingDir: /work
      # tini reaps what a killed command leaves; the commands are exec'd into the container by adam-kube-exec.
      command: ["tini", "--", "sleep", "infinity"]
      securityContext:
        {{- toYaml .Values.containerSecurityContext | nindent 8 }}
      env:
        - name: CARGO_BUILD_JOBS
          value: {{ .Values.runPods.cargoBuildJobs | quote }}
        {{- /* The only way out to the internet (ADR 0042): the egress proxy. The pod's NetworkPolicy allows nothing else, so a tool that ignores these gets no connection. */}}
        {{- range $name := list "HTTPS_PROXY" "https_proxy" "HTTP_PROXY" "http_proxy" }}
        - name: {{ $name }}
          value: {{ include "coder.egressProxyURL" $ | quote }}
        {{- end }}
        {{- range $name := list "NO_PROXY" "no_proxy" }}
        - name: {{ $name }}
          value: {{ include "coder.egressProxyNoProxy" $ | quote }}
        {{- end }}
        {{- range $name, $value := .Values.runPods.extraEnv }}
        - name: {{ $name }}
          value: {{ $value | quote }}
        {{- end }}
        {{- if .Values.externalSecrets.enabled }}
        {{- /* The model key OpenCode reads (secret_ref "model-key"): the one reference to a Secret the admission policy lets a run pod have, by name and key. */}}
        - name: {{ include "coder.runPodSecretKey" . }}
          valueFrom:
            secretKeyRef:
              name: {{ include "coder.secretName" . }}
              key: {{ include "coder.runPodSecretKey" . }}
        {{- end }}
      resources:
        {{- toYaml .Values.runPods.resources | nindent 8 }}
      volumeMounts:
        {{- /* The one mount of the workspace volume, whole: the coder replaces it by the run's own mounts (see above). */}}
        - name: work
          mountPath: /work
        {{- if include "coder.cache" . }}
        {{- /* The package cache, shared with the coder pod and the other run pods (ADR 0040). */}}
        - name: cache
          mountPath: {{ include "coder.cacheDir" . }}
        {{- end }}
        - name: tools
          mountPath: /opt/adam/bin
          readOnly: true
  volumes:
    - name: work
      persistentVolumeClaim:
        claimName: {{ include "coder.runPodWorkClaim" . }}
    {{- if include "coder.cache" . }}
    - name: cache
      persistentVolumeClaim:
        claimName: {{ include "coder.cacheClaim" . }}
    {{- end }}
    - name: tools
      emptyDir: {}
{{- end -}}

{{/* The images a run pod may use, one per line: the run image, the tools image and the extras. */}}
{{- define "coder.runPodAllowedImages" -}}
{{- $images := list (include "coder.runPodImage" .) (include "coder.runPodToolsImage" .) -}}
{{- range .Values.runPods.admissionPolicy.extraAllowedImages -}}
{{- $images = append $images (toString .) -}}
{{- end -}}
{{- range $images | uniq }}
{{ . }}
{{- end -}}
{{- end -}}
