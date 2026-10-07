{{/*
Value checks that must stop a render, not surface as a broken rollout or a quietly different deployment.
Included from deployment.yaml, which every render has, so they always run.
*/}}
{{- define "adam-operator.validate" -}}
{{- /* One replica and no leader election in v0: a second reconciler would only race the first. */ -}}
{{- if or (hasKey .Values "replicaCount") (hasKey .Values "replicas") -}}
{{- fail "the operator has no leader election in v0 and runs one replica: there is no replicaCount" -}}
{{- end -}}
{{- /* The image: a tag nothing moves under it. */ -}}
{{- if not .Values.image.repository -}}
{{- fail "image.repository is required" -}}
{{- end -}}
{{- if not .Values.image.tag -}}
{{- fail "image.tag is required: a sha-<7 hex> tag of the image (CI bumps it)" -}}
{{- end -}}
{{- if eq (toString .Values.image.tag) "latest" -}}
{{- fail "image.tag must not be latest: a moving tag would roll the operator out without a commit" -}}
{{- end -}}
{{- if and .Values.image.digest (not (regexMatch "^sha256:[0-9a-f]{64}$" (toString .Values.image.digest))) -}}
{{- fail (printf "image.digest must be sha256: and 64 hex digits, got %q" (toString .Values.image.digest)) -}}
{{- end -}}
{{- /* The namespace it watches: one, named. */ -}}
{{- if and .Values.watchNamespace (not (regexMatch "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$" (toString .Values.watchNamespace))) -}}
{{- fail (printf "watchNamespace must be one namespace name (a DNS label), got %q: the operator is namespaced and has no value that watches every namespace" (toString .Values.watchNamespace)) -}}
{{- end -}}
{{- if not (kindIs "bool" .Values.storeCnpg) -}}
{{- fail "storeCnpg must be true or false" -}}
{{- end -}}
{{- /* The service account. */ -}}
{{- $_ := include "adam-operator.serviceAccountName" . -}}
{{- /* The registry token: from one place at most, and then the rest of the registry's settings follow. */ -}}
{{- if and .Values.externalSecrets.enabled .Values.registry.tokenSecret.name -}}
{{- fail "registry.tokenSecret.name and externalSecrets.enabled are both set: the token comes from one place" -}}
{{- end -}}
{{- if .Values.registry.tokenSecret.name -}}
{{- if not .Values.registry.tokenSecret.key -}}
{{- fail "registry.tokenSecret.key is required with registry.tokenSecret.name" -}}
{{- end -}}
{{- end -}}
{{- if .Values.externalSecrets.enabled -}}
{{- $es := .Values.externalSecrets -}}
{{- if not $es.key -}}{{- fail "externalSecrets.key is required: the AWS Secrets Manager secret (one JSON property per value)" -}}{{- end -}}
{{- if not $es.secretStoreRef.name -}}{{- fail "externalSecrets.secretStoreRef.name is required" -}}{{- end -}}
{{- if not (has (toString $es.secretStoreRef.kind) (list "ClusterSecretStore" "SecretStore")) -}}
{{- fail (printf "externalSecrets.secretStoreRef.kind must be ClusterSecretStore or SecretStore, got %q" (toString $es.secretStoreRef.kind)) -}}
{{- end -}}
{{- if not $es.properties.registryToken -}}{{- fail "externalSecrets.properties.registryToken is required: the AWS property that holds the registry's token" -}}{{- end -}}
{{- end -}}
{{- if and .Values.registry.publicUrl (not (regexMatch "^https?://[^/@]+(/.*)?$" (toString .Values.registry.publicUrl))) -}}
{{- fail (printf "registry.publicUrl must be an absolute http(s) URL with no credentials, got %q" (toString .Values.registry.publicUrl)) -}}
{{- end -}}
{{- /* The network policy: who reads the registry is said, never implied, and a peer says who. */ -}}
{{- if .Values.networkPolicy.enabled -}}
{{- if and (include "adam-operator.registrySecretName" .) (not .Values.networkPolicy.registry.allowFrom) -}}
{{- fail "networkPolicy.registry.allowFrom is required while the registry has a token and networkPolicy is enabled: with none, nothing could read it (list the orchestrator's namespace or pods, or set networkPolicy.enabled=false)" -}}
{{- end -}}
{{- range $list := list "registry" "metrics" -}}
{{- range $peer := (get $.Values.networkPolicy $list).allowFrom -}}
{{- if not (or (hasKey $peer "namespaceSelector") (hasKey $peer "podSelector") (hasKey $peer "ipBlock")) -}}
{{- fail (printf "networkPolicy.%s.allowFrom: a peer needs a namespaceSelector, a podSelector or an ipBlock, got %s" $list (toJson $peer)) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- if not .Values.networkPolicy.apiServer.ports -}}
{{- fail "networkPolicy.apiServer.ports must name at least one port: the operator reaches nothing else" -}}
{{- end -}}
{{- range $p := .Values.networkPolicy.apiServer.ports -}}
{{- if not (and (or (kindIs "float64" $p) (kindIs "int" $p) (kindIs "int64" $p)) (ge (int $p) 1) (le (int $p) 65535)) -}}
{{- fail (printf "networkPolicy.apiServer.ports: %v is not a port number" $p) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- /* Settings that have a default: a number, above zero. */ -}}
{{- range $k, $v := .Values.tuning -}}
{{- if not (has $k (list "concurrency" "resyncSecs" "resyncPendingSecs")) -}}
{{- fail (printf "tuning.%s is not a setting: concurrency, resyncSecs or resyncPendingSecs" $k) -}}
{{- end -}}
{{- if not (and (or (kindIs "float64" $v) (kindIs "int" $v) (kindIs "int64" $v)) (ge (int $v) 1)) -}}
{{- fail (printf "tuning.%s must be a whole number of at least 1, got %v" $k $v) -}}
{{- end -}}
{{- end -}}
{{- end -}}
