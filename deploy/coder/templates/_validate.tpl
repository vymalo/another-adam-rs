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
{{- if gt (int .Values.replicaCount) 1 -}}
{{- fail "replicaCount > 1 is not supported: runs move between workers at every step, so more than one worker needs workspace placement or a shared /work, or a run forks into a second pull request (see deploy/coder/README.md, Known risks)" -}}
{{- end -}}
{{- end -}}
