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

{{/* The URL clients use for the JSON-RPC endpoint (the agent card advertises it). */}}
{{- define "coder.publicUrl" -}}
{{- if .Values.config.publicUrl -}}
{{- .Values.config.publicUrl -}}
{{- else -}}
{{- printf "http://%s.%s.svc.cluster.local:%d/" (include "coder.fullname" .) .Release.Namespace (int .Values.service.port) -}}
{{- end -}}
{{- end -}}
