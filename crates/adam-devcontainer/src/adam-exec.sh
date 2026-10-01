#!/bin/sh
# adam-exec: starts and stops the processes the coder runs in a devcontainer.
#
# It is written once to <root>/environments/.tools/<version>/adam-exec by adam-devcontainer, mounted
# read-only at /opt/adam/bin, and called through `devcontainer exec` (run, shell) and `podman exec`
# (kill, chown). POSIX sh, no tool beyond what a minimal image has (chown, kill, and a /proc).
#
#   adam-exec run   <id> <cwd> :<program> [:<arg>...]   run a program in cwd
#   adam-exec shell <id> <cwd> :<command line>           run a command line in a login shell
#   adam-exec kill  <id>                                 stop what `run` or `shell` started
#   adam-exec chown <uid> <gid> <dir>                    give a directory tree to <uid>:<gid> (as the host sees them)
#
# Every word of the command after <cwd> carries a leading ":" that is removed here. The devcontainer
# CLI reads its command line with an option parser, which would take an argument such as "--version"
# or "-c" for one of its own; a word that does not start with "-" is left alone.
#
# `run` and `shell` record the process (its id, and its start time, so that a pid that was reused
# is never mistaken for it) in $ADAM_EXEC_DIR (default /tmp/adam-exec) and become the command. `kill`
# stops that process and all of its descendants, found through the parent ids of /proc, which is
# the container's own (a container has its own process namespace). Killing the `devcontainer exec`
# client does not stop what it started inside the container; this does.

set -u

dir=${ADAM_EXEC_DIR:-/tmp/adam-exec}
# Where /proc is (a test points it at a copy).
proc=${ADAM_EXEC_PROC:-/proc}

fail() {
    echo "adam-exec: $*" >&2
    exit "${2:-2}"
}

# An id is what the caller made: letters, digits, dot, dash, underscore.
check_id() {
    case $1 in
    '' | *[!A-Za-z0-9._-]*) fail "bad id" ;;
    esac
}

# The id inside the container of the id $1 outside it, from the map file $2 (lines of
# "<inside> <outside> <count>"); fails when the container has no such id.
inner_id() {
    while read -r inside outside count; do
        if [ "$1" -ge "$outside" ] && [ "$1" -lt $((outside + count)) ]; then
            echo $((inside + $1 - outside))
            return 0
        fi
    done <"$2"
    return 1
}

# The start time of a process (field 22 of /proc/<pid>/stat), empty when it is gone. The command name
# in the second field may hold spaces and parentheses, so cut after its last ") ".
start_time() {
    line=
    read -r line <"$proc/$1/stat" 2>/dev/null || return 0
    rest=${line##*) }
    # shellcheck disable=SC2086 # splitting is the point
    set -- $rest
    shift 19
    echo "${1:-}"
}

# The pid and every descendant of it, one per line. When the pid leads a process group (a command
# started by something that gave it one), the members of that group count as well: a process that
# was started in the background and then lost its parent (a daemon in the making) is found by its
# group, which a walk of parent ids could not. Without that, such a process stays until the
# container is removed.
tree() {
    root=$1
    pairs=
    rootgroup=
    for stat in "$proc"/[0-9]*/stat; do
        line=
        read -r line <"$stat" 2>/dev/null || continue
        p=${stat#"$proc"/}
        p=${p%/stat}
        rest=${line##*) }
        # shellcheck disable=SC2086
        set -- $rest
        pairs="$pairs $p:${2:-0}:${3:-0}"
        [ "$p" = "$root" ] && rootgroup=${3:-}
    done
    all=$root
    frontier=$root
    while [ -n "$frontier" ]; do
        next=
        for pair in $pairs; do
            child=${pair%%:*}
            parent=${pair#*:}
            parent=${parent%%:*}
            for f in $frontier; do
                if [ "$parent" = "$f" ]; then
                    next="$next $child"
                    all="$all $child"
                    break
                fi
            done
        done
        frontier=$next
    done
    if [ -n "$rootgroup" ] && [ "$rootgroup" = "$root" ]; then
        for pair in $pairs; do
            group=${pair##*:}
            child=${pair%%:*}
            if [ "$group" = "$root" ] && [ "$child" != "$root" ]; then
                all="$all $child"
            fi
        done
    fi
    for p in $all; do
        echo "$p"
    done
}

cmd=${1:-}
case $cmd in
run | shell)
    [ $# -ge 4 ] || fail "usage: adam-exec $cmd <id> <cwd> :<command>..."
    id=$2
    cwd=$3
    check_id "$id"
    shift 3
    mkdir -p "$dir" || fail "cannot create $dir" 126
    echo "$$ $(start_time $$)" >"$dir/$id.pid" || fail "cannot record the process" 126
    cd "$cwd" 2>/dev/null || fail "cannot enter $cwd" 126
    if [ "$cmd" = shell ]; then
        line=${1#:}
        if command -v bash >/dev/null 2>&1; then
            exec bash -lc "$line"
        fi
        exec sh -lc "$line"
    fi
    # Remove the marker from every word: take the first, put it behind the others, once for each.
    n=$#
    while [ "$n" -gt 0 ]; do
        word=$1
        shift
        set -- "$@" "${word#:}"
        n=$((n - 1))
    done
    exec "$@"
    ;;
kill)
    [ $# -eq 2 ] || fail "usage: adam-exec kill <id>"
    id=$2
    check_id "$id"
    [ -f "$dir/$id.pid" ] || exit 0
    pid=
    started=
    read -r pid started <"$dir/$id.pid" || true
    rm -f "$dir/$id.pid"
    case $pid in
    '' | *[!0-9]*) exit 0 ;;
    esac
    # A process that is gone, or a pid that now belongs to another process, is not ours to kill.
    [ -n "$started" ] && [ "$(start_time "$pid")" = "$started" ] || exit 0
    # Twice: a process that forked between the walk and the kill leaves a child behind the first time.
    for _ in 1 2; do
        for p in $(tree "$pid"); do
            kill -s KILL "$p" 2>/dev/null || true
        done
    done
    exit 0
    ;;
chown)
    [ $# -eq 4 ] || fail "usage: adam-exec chown <uid> <gid> <dir>"
    for n in "$2" "$3"; do
        case $n in
        '' | *[!0-9]*) fail "bad id" ;;
        esac
    done
    case $4 in
    /*) ;;
    *) fail "the directory must be absolute" ;;
    esac
    [ -d "$4" ] || exit 0
    # The ids are the ones outside the container (the coder's own). Inside, they are whatever the
    # container's id map says: the user's own number under keep-id, root's without it. The map is
    # read here, so that nothing has to guess which of the two this container has.
    inner_uid=$(inner_id "$2" "$proc/self/uid_map") || exit 0
    inner_gid=$(inner_id "$3" "$proc/self/gid_map") || exit 0
    # -h: a symbolic link in the tree is changed itself, never what it points to.
    exec chown -hR "$inner_uid:$inner_gid" "$4"
    ;;
*)
    fail "usage: adam-exec run|shell|kill|chown ..."
    ;;
esac
