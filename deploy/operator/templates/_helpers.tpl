{{/* Names and labels. Objects are named <fullname> or <fullname>-<part>. */}}
{{- define "adam-operator.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "adam-operator.fullname" -}}
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

{{- define "adam-operator.selectorLabels" -}}
app.kubernetes.io/name: {{ include "adam-operator.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "adam-operator.labels" -}}
{{ include "adam-operator.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: adam-rs
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end -}}

{{/* repository:tag, plus @digest when there is one. */}}
{{- define "adam-operator.image" -}}
{{- $i := .Values.image -}}
{{- $tag := required "image.tag is required" $i.tag -}}
{{- if $i.digest -}}
{{- printf "%s:%s@%s" (required "image.repository is required" $i.repository) $tag $i.digest -}}
{{- else -}}
{{- printf "%s:%s" (required "image.repository is required" $i.repository) $tag -}}
{{- end -}}
{{- end -}}

{{/* The namespace the operator watches, and where its Role is: watchNamespace, else the release's. */}}
{{- define "adam-operator.watchNamespace" -}}
{{- default .Release.Namespace .Values.watchNamespace -}}
{{- end -}}

{{- define "adam-operator.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "adam-operator.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- required "serviceAccount.name is required when serviceAccount.create is false" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/* The Secret that holds the registry's token ("" when there is none: no token, no registry). */}}
{{- define "adam-operator.registrySecretName" -}}
{{- if .Values.externalSecrets.enabled -}}
{{- printf "%s-registry" (include "adam-operator.fullname" .) -}}
{{- else -}}
{{- .Values.registry.tokenSecret.name -}}
{{- end -}}
{{- end -}}

{{/* The key of that Secret. */}}
{{- define "adam-operator.registrySecretKey" -}}
{{- if .Values.externalSecrets.enabled -}}token{{- else -}}{{- .Values.registry.tokenSecret.key -}}{{- end -}}
{{- end -}}
