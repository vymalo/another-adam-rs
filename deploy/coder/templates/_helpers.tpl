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

{{/*
Whether this pod authenticates to GitHub as an App installation: github.auth=app, and the role runs workers
(a control plane never talks to GitHub). Renders "true" or nothing, like coder.runsWorkers.
*/}}
{{- define "coder.githubApp" -}}
{{- if and (eq (toString .Values.github.auth) "app") (include "coder.runsWorkers" .) -}}true{{- end -}}
{{- end -}}

{{/*
Whether this pod runs the GitHub MCP server beside the coder (a native sidecar): githubMcp.enabled, and the
role runs workers (a control plane connects no MCP server). Renders "true" or nothing, like coder.runsWorkers.
*/}}
{{- define "coder.githubMcp" -}}
{{- if and .Values.githubMcp.enabled (include "coder.runsWorkers" .) -}}true{{- end -}}
{{- end -}}

{{/*
GITHUB_APP_ID: the application ID or the client ID. A number from a values file is a float64 to Helm
and would print as 1.234567e+06, so numbers go through int64; a string (a client ID, or a quoted ID) is
used as it is.
*/}}
{{- define "coder.githubAppId" -}}
{{- $id := .Values.github.app.id -}}
{{- if kindIs "string" $id -}}{{- trim $id -}}{{- else if gt (int64 $id) 0 -}}{{- int64 $id -}}{{- end -}}
{{- end -}}

{{/*
Whether github.app.installationId is set at all (a pin): anything but empty or null, so that a bad value (0, abc) is
still caught by the validation and is not mistaken for "no pin". Renders "true" or nothing.
*/}}
{{- define "coder.githubAppPinned" -}}
{{- $n := .Values.github.app.installationId -}}
{{- if and (not (kindIs "invalid" $n)) (ne (trim (toString $n)) "") -}}true{{- end -}}
{{- end -}}

{{/* GITHUB_APP_INSTALLATION_ID: a positive integer, from a number or a string; 0 when it is neither. */}}
{{- define "coder.githubAppInstallationId" -}}
{{- $n := .Values.github.app.installationId -}}
{{- if kindIs "string" $n -}}{{- $n = trim $n -}}{{- end -}}
{{- if and (kindIs "string" $n) (not (regexMatch "^[0-9]+$" $n)) -}}0{{- else -}}{{- int64 $n -}}{{- end -}}
{{- end -}}

{{/*
GITHUB_APP_OWNERS: the accounts the App may act for when no installation is pinned, as the list github.app.owners
is (or a string of names separated by commas or spaces), joined with commas. Entries are trimmed and blanks dropped;
`*` is every account the App is installed on (the binary refuses it beside others). Empty when there are none.
*/}}
{{- define "coder.githubAppOwners" -}}
{{- $owners := .Values.github.app.owners -}}
{{- $names := list -}}
{{- if kindIs "string" $owners -}}
{{- range regexSplit "[,[:space:]]+" (trim $owners) -1 -}}
{{- if . -}}{{- $names = append $names . -}}{{- end -}}
{{- end -}}
{{- else -}}
{{- range $owners -}}
{{- $name := trim (toString .) -}}
{{- if $name -}}{{- $names = append $names $name -}}{{- end -}}
{{- end -}}
{{- end -}}
{{- join "," $names -}}
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

{{/*
The workspace placement, trimmed and lower-cased as the binary parses it. Empty
means "not set": the chart sets no WORKSPACE_PLACEMENT and each pod has a volume
of its own, which is today's single-worker render.
*/}}
{{- define "coder.placement" -}}
{{- default "" .Values.workspace.placement | toString | trim | lower -}}
{{- end -}}

{{/* Whether the placement pins runs to a worker (affinity, isolated): renders "true" or nothing. */}}
{{- define "coder.pinsRuns" -}}
{{- if has (include "coder.placement" .) (list "affinity" "isolated") -}}true{{- end -}}
{{- end -}}

{{/* Whether /work is one ReadWriteMany volume for all workers (shared, affinity): "true" or nothing. */}}
{{- define "coder.sharedWork" -}}
{{- if has (include "coder.placement" .) (list "affinity" "shared") -}}true{{- end -}}
{{- end -}}

{{/* Name of the claim behind /work when it is shared: the existing claim, or the one the chart creates. */}}
{{- define "coder.workClaim" -}}
{{- default (printf "%s-work" (include "coder.fullname" .) | trunc 63 | trimSuffix "-") .Values.workspace.sharedVolume.existingClaim -}}
{{- end -}}

{{/*
The extra MCP servers (values `mcp.*`, off by default). websearch is on when its url is set, context7 when
mcp.context7.enabled is true (the string "false" is off). Each renders "true" or nothing. The binary merges
them over the agent's own mcp.json at startup (ADAM_EXTRA_MCP_FILE): the chart ships no copy of the agent's files.
*/}}
{{- define "coder.mcpWebsearch" -}}
{{- if trim (toString .Values.mcp.websearch.url) -}}true{{- end -}}
{{- end -}}

{{- define "coder.mcpContext7" -}}
{{- if eq (toString .Values.mcp.context7.enabled) "true" -}}true{{- end -}}
{{- end -}}

{{/*
Whether this pod gets the extra servers file: one is on, and the role runs workers (only a worker connects an MCP
server). "true" or nothing.
*/}}
{{- define "coder.extraMcp" -}}
{{- if and (or (include "coder.mcpWebsearch" .) (include "coder.mcpContext7" .)) (include "coder.runsWorkers" .) -}}true{{- end -}}
{{- end -}}

{{/* Name of the ConfigMap that holds the extra servers file. */}}
{{- define "coder.extraMcpConfigMapName" -}}
{{- printf "%s-mcp" (include "coder.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/* Where it is mounted, and ADAM_EXTRA_MCP_FILE. */}}
{{- define "coder.extraMcpDir" -}}/etc/adam/extra-mcp{{- end -}}
{{- define "coder.extraMcpFile" -}}/etc/adam/extra-mcp/mcp.json{{- end -}}

{{/*
Whether an extra server's URL is plain http to another machine, which the coder refuses unless the deployment sets
MCP_ALLOW_INSECURE (the same rule as crates/adam-mcp/src/url.rs: https, or http to localhost, *.localhost and loopback).
Takes the URL; renders "true" or nothing.
*/}}
{{- define "coder.plainHttpRemote" -}}
{{- $url := lower (trim (toString .)) -}}
{{- if and (hasPrefix "http://" $url) (not (regexMatch "^http://(localhost|[^/:?#]*\\.localhost|127\\.[0-9.]+|\\[::1\\])([:/?#]|$)" $url)) -}}true{{- end -}}
{{- end -}}

{{/* Whether a server of the extra file is at a plain http URL to another machine. "true" or nothing. */}}
{{- define "coder.mcpPlainHttp" -}}
{{- if or (and (include "coder.mcpWebsearch" .) (include "coder.plainHttpRemote" .Values.mcp.websearch.url)) (and (include "coder.mcpContext7" .) (include "coder.plainHttpRemote" .Values.mcp.context7.url)) -}}true{{- end -}}
{{- end -}}

{{/* Whether the deployment sets MCP_ALLOW_INSECURE itself, in config.extraEnv. "true" or nothing. */}}
{{- define "coder.extraEnvInsecure" -}}
{{- if eq (toString (get .Values.config.extraEnv "MCP_ALLOW_INSECURE")) "true" -}}true{{- end -}}
{{- end -}}

{{/*
Whether the chart sets MCP_ALLOW_INSECURE on the workers: only when websearch is on, mcp.websearch.allowInsecure is
true and extraEnv does not set it already. Never automatic. "true" or nothing.
*/}}
{{- define "coder.mcpInsecure" -}}
{{- if and (include "coder.runsWorkers" .) (include "coder.mcpWebsearch" .) (eq (toString .Values.mcp.websearch.allowInsecure) "true") (not (hasKey .Values.config.extraEnv "MCP_ALLOW_INSECURE")) -}}true{{- end -}}
{{- end -}}

{{/*
The extra servers file: `{"mcpServers": {...}}` with the servers that are on, in the shape of mcp.json. A server's
key is its id: the model sees its tools as `<id>__<tool>`. Credentials are `${VAR}` references, filled from the
environment when a worker starts, never values: SEARCH_MCP_TOKEN and CONTEXT7_API_KEY come from the ExternalSecret.
`optional` is written only when true (the binary's default is required).
*/}}
{{- define "coder.extraMcpJson" -}}
{{- $servers := dict -}}
{{- if include "coder.mcpWebsearch" . -}}
{{- $s := .Values.mcp.websearch -}}
{{- $server := dict "type" "http" "url" (trim $s.url) "headers" (dict $s.header (printf "%s${SEARCH_MCP_TOKEN}" (toString $s.valuePrefix))) -}}
{{- if $s.tools -}}{{- $_ := set $server "tools" $s.tools -}}{{- end -}}
{{- if $s.optional -}}{{- $_ := set $server "optional" true -}}{{- end -}}
{{- $_ := set $servers "websearch" $server -}}
{{- end -}}
{{- if include "coder.mcpContext7" . -}}
{{- $s := .Values.mcp.context7 -}}
{{- $server := dict "type" "http" "url" (trim $s.url) "headers" (dict $s.header (printf "%s${CONTEXT7_API_KEY}" (toString $s.valuePrefix))) -}}
{{- if $s.tools -}}{{- $_ := set $server "tools" $s.tools -}}{{- end -}}
{{- if $s.optional -}}{{- $_ := set $server "optional" true -}}{{- end -}}
{{- $_ := set $servers "context7" $server -}}
{{- end -}}
{{- toPrettyJson (dict "mcpServers" $servers) -}}
{{- end -}}
