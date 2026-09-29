{{/* Names and labels. */}}
{{- define "coder.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "coder.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "coder.selectorLabels" -}}
app.kubernetes.io/name: {{ include "coder.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "coder.labels" -}}
{{ include "coder.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end -}}

{{/* Name of the Secret the ExternalSecret creates. */}}
{{- define "coder.secretName" -}}
{{- default (include "coder.fullname" .) .Values.externalSecrets.targetSecretName -}}
{{- end -}}

{{/* Name of the CNPG Cluster; CNPG names its app Secret <cluster>-app. */}}
{{- define "coder.dbName" -}}
{{- printf "%s-db" (include "coder.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Whether this pod runs workers: every role but `control-plane` (an empty role is
`all`). The role is trimmed and lower-cased first, as the binary parses it, so
" Control-Plane " is a control plane here too. Renders "true" or nothing, so
`{{ if include "coder.runsWorkers" . }}` works. Only workers need the model, GitHub and workspace settings; a control
plane starts runs without them (see docs/architecture.md, "Roles").
*/}}
{{- define "coder.runsWorkers" -}}
{{- if ne (default "" .Values.config.role | trim | lower) "control-plane" -}}true{{- end -}}
{{- end -}}

{{/* The URL clients use for the JSON-RPC endpoint (the agent card advertises it). */}}
{{- define "coder.publicUrl" -}}
{{- if .Values.config.publicUrl -}}
{{- .Values.config.publicUrl -}}
{{- else -}}
{{- printf "http://%s.%s.svc.cluster.local:%d/" (include "coder.fullname" .) .Release.Namespace (int .Values.service.port) -}}
{{- end -}}
{{- end -}}

{{/*
topology=split: the front is its own Deployment. Its pods carry a different
`app.kubernetes.io/name` (<name>-front) from the worker StatefulSet's, so the
StatefulSet's selector (name + instance, immutable) never matches a front pod,
and a helm upgrade between the two topologies keeps the StatefulSet as it is.
*/}}
{{- define "coder.frontName" -}}
{{- printf "%s-front" (include "coder.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "coder.frontSelectorLabels" -}}
app.kubernetes.io/name: {{ printf "%s-front" (include "coder.name" .) | trunc 63 | trimSuffix "-" }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: front
{{- end -}}

{{- define "coder.frontLabels" -}}
{{ include "coder.frontSelectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end -}}
