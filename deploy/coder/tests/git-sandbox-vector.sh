#!/bin/sh
# A scratch project whose own configuration runs a program, the vector the git sandbox exists for (ADR 0041, open decision 6;
# issue vymalo/another-adam-rs#118). tests/kind-run-pods.sh feeds this to `sh -s` in the coder container of a pod shaped like the
# chart's coder pod, then lets git add the project's files twice: through the git sandbox (adam-git) and as the coder's own git
# (the control). It writes only files; nothing here runs the program.
#
#   sh git-sandbox-vector.sh <directory> [<what the program tries to read>...]
#
# <directory>/repo is a git repository with, in its own .git/config, a `filter.x.clean` and a `core.fsmonitor` that name
# <directory>/probe.sh, and a .gitattributes that applies the filter to every file. probe.sh is what a hostile configuration
# would run: it writes, to <directory>/marker, who it ran as, the first process of its container, what it could read of each
# file named on the command line of this script (default: the service-account token and the GitHub App's key), the environment of
# every process it can open, and its own environment. Then it passes its input through, so that git goes on. A program that ran
# in a place with no secret reads nothing of them, and that is what the marker shows.
set -eu

dir=${1:?usage: git-sandbox-vector.sh <directory> [<file the program tries to read>...]}
shift
if [ "$#" -eq 0 ]; then
  set -- /var/run/secrets/kubernetes.io/serviceaccount/token /var/run/secrets/github-app/private-key.pem
fi

rm -rf "$dir"
mkdir -p "$dir/repo"

printf '%s\n' "$@" > "$dir/reads"
cat > "$dir/probe.sh" <<'PROBE'
#!/bin/sh
# What a hostile filter or fsmonitor would try. It appends to the marker next to it and then passes its input through.
here=$(dirname "$0")
{
  echo "== ran as uid $(id -u), first process: $(tr '\0' ' ' < /proc/1/cmdline)"
  while read -r file; do
    echo "== file $file"
    cat "$file" 2>&1
  done < "$here/reads"
  for environ in /proc/[0-9]*/environ; do
    echo "== $environ"
    tr '\0' '\n' < "$environ" 2>&1
  done
  echo "== its own environment"
  env
} >> "$here/marker" 2>&1
# A filter gets one argument (%f) and its input on stdin; git calls an fsmonitor with two and expects a list.
if [ "$#" -eq 1 ]; then cat; fi
PROBE
chmod 0755 "$dir/probe.sh"

git init -q -b main "$dir/repo"
cat >> "$dir/repo/.git/config" <<CONFIG
[filter "x"]
	clean = $dir/probe.sh %f
[core]
	fsmonitor = $dir/probe.sh
CONFIG
printf '* filter=x\n' > "$dir/repo/.gitattributes"
printf 'hello\n' > "$dir/repo/a.txt"
