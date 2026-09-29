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
{{- $placement := include "coder.placement" . -}}
{{- if eq $placement "a2a-only" -}}
{{- fail "workspace.placement a2a-only is refused for the coder: every tool of the coder needs a workspace (a2a-only is for hosts whose agents only call remote agents). Use shared, affinity or isolated" -}}
{{- end -}}
{{- if not (has $placement (list "" "shared" "affinity" "isolated")) -}}
{{- fail (printf "workspace.placement must be one of shared, affinity or isolated (or empty for the single-worker default), got %q" $placement) -}}
{{- end -}}
{{- if and (gt (int .Values.replicaCount) 1) (include "coder.runsWorkers" .) (not $placement) -}}
{{- fail "replicaCount > 1 needs workspace.placement (shared, affinity or isolated): runs move between workers at every step, and without a placement a run that lands on a worker without its worktree forks into a second pull request (see deploy/coder/README.md, Workspace placement)" -}}
{{- end -}}
{{- if and (has $placement (list "shared" "affinity")) (not .Values.workspace.sharedVolume.existingClaim) (not .Values.workspace.sharedVolume.storageClass) -}}
{{- fail (printf "workspace.placement=%s mounts one ReadWriteMany volume: set workspace.sharedVolume.storageClass (a class that supports ReadWriteMany) or workspace.sharedVolume.existingClaim" $placement) -}}
{{- end -}}
{{- end -}}
