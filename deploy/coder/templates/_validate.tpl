{{/*
Value checks that must stop a render, not surface as a broken rollout. Included
from service.yaml, which every render contains, so they always run.
*/}}
{{- define "coder.validate" -}}
{{- if not (has .Values.topology (list "combined" "split")) -}}
{{- fail (printf "topology must be combined or split, got %q" (toString .Values.topology)) -}}
{{- end -}}
{{- if and (eq .Values.topology "split") .Values.config.role -}}
{{- fail "config.role is for topology=combined; topology=split sets ROLE itself (control-plane on the front, worker on the StatefulSet)" -}}
{{- end -}}
{{- $dbSecret := default "" .Values.database.existingSecret.name | toString | trim -}}
{{- if and .Values.database.enabled $dbSecret -}}
{{- fail "database.enabled=true and database.existingSecret.name are both set: the chart creates its own Cluster, or it reads an existing database's Secret, not both" -}}
{{- end -}}
{{- if and (not .Values.database.enabled) (not $dbSecret) -}}
{{- fail "database.enabled=false needs database.existingSecret.name (a Secret in the release namespace whose key, `uri` by default, holds the Postgres connection string): every pod needs DATABASE_URL" -}}
{{- end -}}
{{- $placement := include "coder.placement" . -}}
{{- if eq $placement "a2a-only" -}}
{{- fail "workspace.placement a2a-only is refused for the coder: every tool of the coder needs a workspace (a2a-only is for hosts whose agents only call remote agents). Use shared, affinity or isolated" -}}
{{- end -}}
{{- if not (has $placement (list "" "shared" "affinity" "isolated")) -}}
{{- fail (printf "workspace.placement must be one of shared, affinity or isolated (or empty for the single-worker default), got %q" $placement) -}}
{{- end -}}
{{- if not (has (toString .Values.github.auth) (list "token" "app")) -}}
{{- fail (printf "github.auth must be token or app, got %q" (toString .Values.github.auth)) -}}
{{- end -}}
{{- if include "coder.githubApp" . -}}
{{- if not (include "coder.githubAppId" .) -}}
{{- fail "github.auth=app needs github.app.id (the App's application ID or client ID)" -}}
{{- end -}}
{{- $pinned := include "coder.githubAppPinned" . -}}
{{- $owners := include "coder.githubAppOwners" . -}}
{{- if and $pinned $owners -}}
{{- fail "github.app.installationId and github.app.owners are both set: pin one installation, or list the accounts the App may act for (an installation is found for each), not both" -}}
{{- end -}}
{{- if not (or $pinned $owners) -}}
{{- fail "github.auth=app needs github.app.installationId (one installation, a positive integer) or github.app.owners (the accounts the App may act for: the installation of each is found)" -}}
{{- end -}}
{{- if and $pinned (le (int64 (include "coder.githubAppInstallationId" .)) 0) -}}
{{- fail "github.app.installationId must be a positive integer" -}}
{{- end -}}
{{- if not .Values.github.app.privateKeySecret -}}
{{- fail "github.auth=app needs github.app.privateKeySecret: the name of a Secret with the App's private key under the key private-key.pem" -}}
{{- end -}}
{{- end -}}
{{- if .Values.githubMcp.enabled -}}
{{- $port := int64 .Values.githubMcp.port -}}
{{- if or (lt $port 1) (gt $port 65535) -}}
{{- fail (printf "githubMcp.port must be a port number (1 to 65535), got %q" (toString .Values.githubMcp.port)) -}}
{{- end -}}
{{- end -}}
{{- /* The extra MCP servers (values `mcp.*`): a file the binary merges over the agent's own mcp.json. */ -}}
{{- range $path, $value := dict "mcp.context7.enabled" .Values.mcp.context7.enabled "mcp.websearch.allowInsecure" .Values.mcp.websearch.allowInsecure -}}
{{- if not (has (toString $value) (list "true" "false")) -}}
{{- fail (printf "%s must be true or false, got %q" $path (toString $value)) -}}
{{- end -}}
{{- end -}}
{{- if or (include "coder.mcpWebsearch" .) (include "coder.mcpContext7" .) -}}
{{- if hasKey .Values.config.extraEnv "ADAM_EXTRA_MCP_FILE" -}}
{{- fail "config.extraEnv.ADAM_EXTRA_MCP_FILE is set while mcp.websearch.url or mcp.context7.enabled is on: the chart then sets it itself, to the file it mounts" -}}
{{- end -}}
{{- range $id, $on := dict "websearch" (include "coder.mcpWebsearch" .) "context7" (include "coder.mcpContext7" .) -}}
{{- if $on -}}
{{- $s := get $.Values.mcp $id -}}
{{- $url := trim (toString $s.url) -}}
{{- if not (regexMatch "^https?://[^/@?#[:space:]$]+([/?][^[:space:]$]*)?$" $url) -}}
{{- fail (printf "mcp.%s.url must be an http or https URL with no user name, password or ${VAR} in it (a secret in a URL reaches the logs), got %q" $id $url) -}}
{{- end -}}
{{- if not (regexMatch "^[!#$%&'*+.^_`|~0-9A-Za-z-]+$" (toString $s.header)) -}}
{{- fail (printf "mcp.%s.header must be an HTTP header name, got %q" $id (toString $s.header)) -}}
{{- end -}}
{{- if contains "${" (toString $s.valuePrefix) -}}
{{- fail (printf "mcp.%s.valuePrefix must be plain text such as \"Bearer \": the token is added by the chart, from the Secret" $id) -}}
{{- end -}}
{{- if not (kindIs "slice" $s.tools) -}}
{{- fail (printf "mcp.%s.tools must be a list of tool names (for example [web_search]), got %q" $id (toString $s.tools)) -}}
{{- end -}}
{{- if not (kindIs "bool" $s.optional) -}}
{{- fail (printf "mcp.%s.optional must be true or false, got %q" $id (toString $s.optional)) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- if and (include "coder.mcpPlainHttp" .) (hasKey .Values.config.extraEnv "MCP_ALLOW_INSECURE") (not (include "coder.extraEnvInsecure" .)) -}}
{{- fail (printf "config.extraEnv.MCP_ALLOW_INSECURE is %q while an extra MCP server is at a plain http:// URL on another machine: the coder would refuse that server at startup. Set it to \"true\" (knowing that it covers every MCP server and the thread-tools endpoints), remove it, or use an https URL" (toString (get .Values.config.extraEnv "MCP_ALLOW_INSECURE"))) -}}
{{- end -}}
{{- if and (include "coder.mcpPlainHttp" .) (not (eq (toString .Values.mcp.websearch.allowInsecure) "true")) (not (include "coder.extraEnvInsecure" .)) -}}
{{- fail "an extra MCP server is at a plain http:// URL on another machine, which the coder refuses unless MCP_ALLOW_INSECURE is set: set mcp.websearch.allowInsecure=true (or config.extraEnv.MCP_ALLOW_INSECURE=\"true\") knowing that it covers every MCP server of the agent and the thread-tools endpoints senders announce, and that the bearer crosses the network in the clear; or use an https URL" -}}
{{- end -}}
{{- if include "coder.runsWorkers" . -}}
{{- if not .Values.externalSecrets.enabled -}}
{{- fail "mcp.websearch.url or mcp.context7.enabled is on, and their keys come from the ExternalSecret only: set externalSecrets.enabled=true (a key is never a chart value)" -}}
{{- end -}}
{{- if and (include "coder.mcpWebsearch" .) (not .Values.externalSecrets.properties.searchMcpToken) -}}
{{- fail "mcp.websearch.url is set: externalSecrets.properties.searchMcpToken must name the AWS property of its bearer token" -}}
{{- end -}}
{{- if and (include "coder.mcpContext7" .) (not .Values.externalSecrets.properties.context7ApiKey) -}}
{{- fail "mcp.context7.enabled is true: externalSecrets.properties.context7ApiKey must name the AWS property of its API key" -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- /* config.modelExtraBody: a JSON object (a map, or a string that parses to one); config.modelEchoReasoning: a member name. */ -}}
{{- with include "coder.modelExtraBody" . -}}
{{- $parsed := fromJson . -}}
{{- if or (hasKey $parsed "Error") (not (kindIs "map" $parsed)) -}}
{{- fail "config.modelExtraBody must be a JSON object (a map, or a string like {\"reasoning_effort\":\"medium\"})" -}}
{{- end -}}
{{- if hasKey $.Values.config.extraEnv "MODEL_EXTRA_BODY" -}}
{{- fail "config.modelExtraBody is set and config.extraEnv sets MODEL_EXTRA_BODY: set one" -}}
{{- end -}}
{{- range $key := list "model" "messages" "tools" "tool_choice" "stream" -}}
{{- if hasKey $parsed $key -}}
{{- fail (printf "config.modelExtraBody may not set %q: the runtime owns it" $key) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- if and .Values.config.modelEchoReasoning (not (has (toString .Values.config.modelEchoReasoning) (list "reasoning_content" "reasoning"))) -}}
{{- fail (printf "config.modelEchoReasoning must be empty, reasoning_content or reasoning, got %q" (toString .Values.config.modelEchoReasoning)) -}}
{{- end -}}
{{- if and .Values.config.modelEchoReasoning (hasKey .Values.config.extraEnv "MODEL_ECHO_REASONING") -}}
{{- fail "config.modelEchoReasoning is set and config.extraEnv sets MODEL_ECHO_REASONING: set one" -}}
{{- end -}}
{{- /* config.modelBaseUrlFromSecret: MODEL_BASE_URL from the ExternalSecret, not from config.modelBaseUrl. */ -}}
{{- if not (kindIs "bool" .Values.config.modelBaseUrlFromSecret) -}}
{{- fail (printf "config.modelBaseUrlFromSecret must be true or false, got %q" (toString .Values.config.modelBaseUrlFromSecret)) -}}
{{- end -}}
{{- if include "coder.modelBaseUrlFromSecret" . -}}
{{- if include "coder.modelBaseUrlLiteral" . -}}
{{- fail "config.modelBaseUrlFromSecret is on and config.modelBaseUrl is set: the gateway's URL comes from the ExternalSecret then, so a URL in values would be written in git and ignored. Remove config.modelBaseUrl (the default placeholder, or empty, counts as unset), or turn the option off" -}}
{{- end -}}
{{- if hasKey .Values.config.extraEnv "MODEL_BASE_URL" -}}
{{- fail "config.modelBaseUrlFromSecret is on and config.extraEnv sets MODEL_BASE_URL: the chart sets it itself, from the ExternalSecret. Remove it from config.extraEnv" -}}
{{- end -}}
{{- if not .Values.externalSecrets.enabled -}}
{{- fail "config.modelBaseUrlFromSecret is on and its value comes from the ExternalSecret only: set externalSecrets.enabled=true, or turn the option off and set config.modelBaseUrl" -}}
{{- end -}}
{{- if not .Values.externalSecrets.properties.modelBaseUrl -}}
{{- fail "config.modelBaseUrlFromSecret is on: externalSecrets.properties.modelBaseUrl must name the AWS property that holds the gateway's URL (model_base_url)" -}}
{{- end -}}
{{- end -}}
{{- /* runPods (ADR 0019): a pod of its own for each active run. */ -}}
{{- if not (kindIs "bool" .Values.runPods.enabled) -}}
{{- fail (printf "runPods.enabled must be true or false, got %q" (toString .Values.runPods.enabled)) -}}
{{- end -}}
{{- if include "coder.runPods" . -}}
{{- $rp := .Values.runPods -}}
{{- if not .Values.externalSecrets.enabled -}}
{{- fail "runPods.enabled needs externalSecrets.enabled=true: the model key reaches a run pod only as MODEL_API_KEY from the ExternalSecret's Secret, which the admission policy lets a run pod read, and no GitHub credential ever does" -}}
{{- end -}}
{{- if and (gt (int .Values.replicaCount) 1) (not (include "coder.sharedWork" .)) -}}
{{- fail "runPods.enabled with replicaCount > 1 needs workspace.placement shared or affinity (a ReadWriteMany volume): a run pod mounts one claim, and the per-pod ReadWriteOnce claim of a coder pod cannot serve the runs of the others" -}}
{{- end -}}
{{- if not (kindIs "bool" $rp.admissionPolicy.enabled) -}}
{{- fail (printf "runPods.admissionPolicy.enabled must be true or false, got %q" (toString $rp.admissionPolicy.enabled)) -}}
{{- end -}}
{{- if and (not $rp.admissionPolicy.enabled) (not (eq (toString $rp.admissionPolicy.disableAcknowledged) "true")) -}}
{{- fail "runPods.admissionPolicy.enabled=false leaves the coder able to make any pod in its namespace (mount the host, read any Secret, run privileged): the admission policy is the guard that makes the RBAC safe. Keep it, or set runPods.admissionPolicy.disableAcknowledged=true to say you know" -}}
{{- end -}}
{{- if and $rp.admissionPolicy.enabled (semverCompare "<1.30.0-0" .Capabilities.KubeVersion.Version) -}}
{{- fail (printf "runPods needs Kubernetes 1.30 or later for its ValidatingAdmissionPolicy (admissionregistration.k8s.io/v1), and this cluster is %s" .Capabilities.KubeVersion.Version) -}}
{{- end -}}
{{- if not (and $rp.image.repository $rp.image.tag) -}}
{{- fail "runPods.image.repository and runPods.image.tag are required (and runPods.image.digest, which the chart ships: an image is pinned by tag and digest)" -}}
{{- end -}}
{{- if and $rp.image.digest (not (regexMatch "^sha256:[0-9a-f]{64}$" (toString $rp.image.digest))) -}}
{{- fail (printf "runPods.image.digest must be sha256:<64 hex digits>, got %q" (toString $rp.image.digest)) -}}
{{- end -}}
{{- if not (regexMatch "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$" (toString $rp.container)) -}}
{{- fail (printf "runPods.container must be a container name (lowercase letters, digits and -), got %q" (toString $rp.container)) -}}
{{- end -}}
{{- if not (get (get $rp.resources "limits" | default dict) "memory") -}}
{{- fail "runPods.resources.limits.memory is required: the quota counts it, and a pod without a memory limit is refused by it" -}}
{{- end -}}
{{- if not (regexMatch "^[0-9]+(\\.[0-9]+)?(Ki|Mi|Gi|Ti|k|M|G|T)?$" (toString $rp.quota.limitsMemory)) -}}
{{- fail (printf "runPods.quota.limitsMemory must be a quantity such as 8Gi, got %q" (toString $rp.quota.limitsMemory)) -}}
{{- end -}}
{{- if lt (int64 $rp.quota.pods) 1 -}}
{{- fail (printf "runPods.quota.pods must be at least 1, got %q" (toString $rp.quota.pods)) -}}
{{- end -}}
{{- range $name, $v := dict "idleSecs" $rp.idleSecs "waitSecs" $rp.waitSecs "readyTimeoutSecs" $rp.readyTimeoutSecs "cargoBuildJobs" $rp.cargoBuildJobs -}}
{{- if not (regexMatch "^[0-9]+$" (toString $v)) -}}
{{- fail (printf "runPods.%s must be a whole number of at least 0, got %q" $name (toString $v)) -}}
{{- end -}}
{{- end -}}
{{- if lt (int64 $rp.readyTimeoutSecs) 1 -}}
{{- fail "runPods.readyTimeoutSecs must be at least 1" -}}
{{- end -}}
{{- if not (kindIs "map" $rp.extraEnv) -}}
{{- fail "runPods.extraEnv must be a name -> value map" -}}
{{- end -}}
{{- if hasKey $rp.extraEnv "MODEL_API_KEY" -}}
{{- fail "runPods.extraEnv sets MODEL_API_KEY: the chart sets it from the ExternalSecret, and a secret is never a chart value" -}}
{{- end -}}
{{- if not (eq (toString $rp.networkPolicy.enabled) "false") -}}
{{- range concat $rp.networkPolicy.blockedCIDRs $rp.networkPolicy.clusterCIDRs -}}
{{- if not (regexMatch "^[0-9]{1,3}(\\.[0-9]{1,3}){3}/[0-9]{1,2}$" (toString .)) -}}
{{- fail (printf "runPods.networkPolicy.blockedCIDRs and clusterCIDRs take IPv4 CIDRs such as 10.0.0.0/8, got %q" (toString .)) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- if and (gt (int .Values.replicaCount) 1) (include "coder.runsWorkers" .) (not $placement) -}}
{{- fail "replicaCount > 1 needs workspace.placement (shared, affinity or isolated): runs move between workers at every step, and without a placement a run that lands on a worker without its worktree forks into a second pull request (see deploy/coder/README.md, Workspace placement)" -}}
{{- end -}}
{{- if and (has $placement (list "shared" "affinity")) (not .Values.workspace.sharedVolume.existingClaim) (not .Values.workspace.sharedVolume.storageClass) -}}
{{- fail (printf "workspace.placement=%s mounts one ReadWriteMany volume: set workspace.sharedVolume.storageClass (a class that supports ReadWriteMany) or workspace.sharedVolume.existingClaim" $placement) -}}
{{- end -}}
{{- end -}}
