#!/bin/sh
# The dev stack's hook program (ADR 0050, slices P1 and P2): it speaks `adam-hooks/1` on stdio and
# refuses the coder's `commit_and_push`, so a task that would push stops at that call.
#
# `dev/hooks/hooks.json` declares it, `dev/compose.hooks.yaml` gives the coder that file
# (`ADAM_HOOKS_FILE`) and `HOOKS_ALLOW_PROGRAMS=true`, and `dev/hooks-e2e.sh` is the test.
#
# The protocol is one JSON object per line on stdin and stdout: the hello is answered with a hello,
# every `tool.before` event whose tool is `commit_and_push` with `deny`, and any other event with
# `continue`. No jq: the protocol's lines are flat and small, and the image's shell and `printf`
# are enough (a flush per line, so the adapter's deadline is never spent in a buffer).

set -eu

# What the denied call's step records and the model reads. `dev/hooks-e2e.sh` matches this line:
# keep it one line and its words.
reason="dev stack: the deployment's hook program denies commit_and_push (no push here)"

while IFS= read -r line; do
  case $line in
    *'"type":"hello"'*)
      printf '%s\n' '{"protocol":"adam-hooks/1","name":"no-git-push","version":1}'
      ;;
    *'"type":"shutdown"'*)
      exit 0
      ;;
    *'"type":"event"'*)
      case $line in
        *'"id":"'*) ;;
        *)
          # Not a line this program can answer: the adapter reads no reply and refuses the call,
          # which is the fail-closed answer it would give anyway.
          continue
          ;;
      esac
      id=${line#*'"id":"'}
      id=${id%%'"'*}
      case $line in
        *'"point":"tool.before"'*'"tool":"commit_and_push"'*)
          printf '{"id":"%s","verdict":"deny","reason":"%s"}\n' "$id" "$reason"
          ;;
        *)
          printf '{"id":"%s","verdict":"continue"}\n' "$id"
          ;;
      esac
      ;;
  esac
done
