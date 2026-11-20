#!/bin/sh
# What a transcript-shaped run costs, on disk and on the wire: 50 commits, each appending a 10 KB
# message, before and after the P2 slices. S0 records the baseline; S3 shows the drop: the contract
# writes the head and the body delta (the `body` table/collection) instead of the whole state in
# `runs`.
#
# The workload is `counting::measure_transcript_write` (crates/adam-store-testkit/src/counting.rs),
# run by the `measure_the_transcript_write` test in each adapter's test file: one run, 50 commits,
# each appending a 10 KB message, through the real adapter and a `CountingStore`. This script
# brackets it with the database's own counters and prints one table per store. It is run by hand
# and quoted in the pull request: CI does not call it, and a store whose variable is unset is
# skipped with a line, so it is a no-op on a machine without the databases.
#
#   ADAM_TEST_POSTGRES_URL=postgres://postgres:postgres@localhost:5432/adam_test \
#   ADAM_TEST_MONGODB_URI=mongodb://localhost:27017 \
#     dev/p2-measure.sh
#
#   dev/p2-measure.sh postgres    # one store (also: mongo; the default is both)
#
# The compose stack serves both (`docker compose up -d --wait postgres mongodb`). Use a fresh
# database for absolute sizes; on a used one the before/after delta is the measurement, and a
# concurrent writer's bytes land in the WAL delta too. PostgreSQL reports `pg_stat_wal` from 14 on;
# an older server prints `n/a` for that row.
#
# psql and mongosh are used when they are on PATH; otherwise the script runs them inside the
# compose services, for a URL that points at localhost. The first run compiles the adapter.
#
# Environment:
#   ADAM_TEST_POSTGRES_URL    the database the Postgres adapter writes to
#   ADAM_TEST_TABLE_PREFIX    its table prefix (default adam_test_, the conformance default)
#   ADAM_TEST_MONGODB_URI     the server the MongoDB adapter connects to, as MongoStore::connect takes it
#   ADAM_TEST_MONGODB_DB      its database (default adam_test)
#   ADAM_TEST_MONGO_COLLECTION_PREFIX  its collection prefix (default adam_)
#   CARGO_TARGET_DIR          inherited by cargo, as everywhere else
set -eu

cd "$(dirname "$0")/.."

stores=${1:-both}
case $stores in
    postgres | mongo | both) ;;
    *)
        echo "usage: $0 [postgres|mongo|both]" >&2
        exit 2
        ;;
esac

prefix=${ADAM_TEST_TABLE_PREFIX:-adam_test_}
mongo_db=${ADAM_TEST_MONGODB_DB:-adam_test}
mongo_prefix=${ADAM_TEST_MONGO_COLLECTION_PREFIX:-adam_}
case $prefix in
    '' | *[!a-z0-9_]*)
        echo "ADAM_TEST_TABLE_PREFIX must be non-empty and match [a-z0-9_]+" >&2
        exit 2
        ;;
esac
case $mongo_prefix in
    '' | *[!a-zA-Z0-9_-]*)
        echo "ADAM_TEST_MONGO_COLLECTION_PREFIX must be non-empty and match [a-zA-Z0-9_-]+" >&2
        exit 2
        ;;
esac

if [ -z "${ADAM_TEST_POSTGRES_URL:-}" ] && [ -z "${ADAM_TEST_MONGODB_URI:-}" ]; then
    echo "p2-measure: neither ADAM_TEST_POSTGRES_URL nor ADAM_TEST_MONGODB_URI is set; nothing to measure"
    exit 0
fi

fail=0
logical=

row() {
    printf '  %-40s %14s %14s %14s\n' "$1" "$2" "$3" "$4"
}

header() {
    printf '\n%s\n' "$1"
    printf '  %-40s %14s %14s %14s\n' "metric (bytes)" "before" "after" "delta"
}

delta() {
    if [ -z "$1" ] || [ -z "$2" ]; then
        printf 'n/a'
        return
    fi
    case $1$2 in
        *[!0-9]*) printf 'n/a' ;;
        *) printf '%s' "$(($2 - $1))" ;;
    esac
}

host_of() {
    printf '%s\n' "$1" | sed -e 's|^[^:]*://||' -e 's|/.*$||' -e 's|^.*@||' -e 's|:.*$||'
}

db_of() {
    printf '%s\n' "$1" | sed -n 's|^[^:]*://[^/]*/\([^?]*\).*$|\1|p'
}

pg_sql() {
    if command -v psql >/dev/null 2>&1; then
        psql "$ADAM_TEST_POSTGRES_URL" -AtX -q -v ON_ERROR_STOP=1 -c "$1"
        return
    fi
    case $(host_of "$ADAM_TEST_POSTGRES_URL") in
        localhost | 127.0.0.1)
            db=$(db_of "$ADAM_TEST_POSTGRES_URL")
            docker compose exec -T postgres \
                psql -U postgres -d "${db:-adam_test}" -AtX -q -v ON_ERROR_STOP=1 -c "$1"
            ;;
        *)
            echo "psql is not on PATH and ADAM_TEST_POSTGRES_URL is not the compose postgres" >&2
            return 1
            ;;
    esac
}

pg_runs() {
    pg_sql "SELECT coalesce(pg_relation_size(to_regclass('${prefix}runs')), 0)"
}

pg_journal() {
    pg_sql "SELECT coalesce(pg_relation_size(to_regclass('${prefix}journal')), 0)"
}

pg_runs_total() {
    pg_sql "SELECT coalesce(pg_total_relation_size(to_regclass('${prefix}runs')), 0)"
}

pg_body() {
    pg_sql "SELECT coalesce(pg_total_relation_size(to_regclass('${prefix}body')), 0)"
}

pg_wal() {
    pg_sql "SELECT wal_bytes FROM pg_stat_wal" 2>/dev/null || true
}

mongo_eval() {
    base=${ADAM_TEST_MONGODB_URI%%\?*}
    case $base in
        *//*/?*) base=${base%/*} ;;
    esac
    uri=${base%/}/$mongo_db
    if command -v mongosh >/dev/null 2>&1; then
        mongosh "$uri" --quiet --eval "$1"
        return
    fi
    case $(host_of "$ADAM_TEST_MONGODB_URI") in
        localhost | 127.0.0.1)
            docker compose exec -T mongodb mongosh "$uri" --quiet --eval "$1"
            ;;
        *)
            echo "mongosh is not on PATH and ADAM_TEST_MONGODB_URI is not the compose mongodb" >&2
            return 1
            ;;
    esac
}

mongo_storage() {
    # The bytes allocated to the collection, free space included: the retained-size counterpart of
    # `pg_total_relation_size`. The workload purges its run when it is done, so `size` (live
    # documents) reads 0 and this is the number that keeps the bytes the run wrote.
    mongo_eval "print(db.getCollectionNames().includes('$1') ? db.getCollection('$1').stats().storageSize : 0)"
}

mongo_written() {
    # The block manager's counter advances when WiredTiger flushes (a journal write or a
    # checkpoint), not when the document is updated in cache: force the flush so the delta covers
    # the workload and not whatever the checkpoint interval happened to catch. Two counters are
    # printed: the data pages the block manager wrote, and the journal ("log bytes written"), which
    # is MongoDB's WAL equivalent.
    mongo_eval 'db.adminCommand({ fsync: 1 }); const w = db.serverStatus().wiredTiger; print((w ? w["block-manager"]["bytes written"] : 0) + " " + (w && w.log ? w.log["log bytes written"] : 0))'
}

# Run the measurement and keep the line it prints; `logical` is read by the caller. A test that ran
# but printed no line is a failure: a rename must not silently measure nothing.
workload() {
    log=$(mktemp)
    if cargo test -p "$1" --test conformance measure_the_transcript_write \
        --locked -- --exact --nocapture >"$log" 2>&1; then
        logical=$(sed -n 's/^p2-measure: //p' "$log" | tail -n 1)
        if [ -z "$logical" ]; then
            fail=1
            echo "  no p2-measure: line in the test output; is the test still named measure_the_transcript_write?" >&2
        fi
    else
        fail=1
        echo "  the workload failed; the numbers below cover a partial run:" >&2
        tail -n 5 "$log" >&2
        logical=
    fi
    rm -f "$log"
}

measure_postgres() {
    if [ -z "${ADAM_TEST_POSTGRES_URL:-}" ]; then
        echo "postgres: skipped (ADAM_TEST_POSTGRES_URL is not set)"
        return
    fi
    if ! pg_sql "SELECT 1" >/dev/null; then
        echo "postgres: FAILED (cannot query $(host_of "$ADAM_TEST_POSTGRES_URL"))" >&2
        fail=1
        return
    fi
    header "postgres $(host_of "$ADAM_TEST_POSTGRES_URL"), tables ${prefix}runs, ${prefix}body, ${prefix}journal"
    runs_before=$(pg_runs)
    body_before=$(pg_body)
    journal_before=$(pg_journal)
    total_before=$(pg_runs_total)
    wal_before=$(pg_wal)
    workload adam-store-postgres
    runs_after=$(pg_runs)
    body_after=$(pg_body)
    journal_after=$(pg_journal)
    total_after=$(pg_runs_total)
    wal_after=$(pg_wal)
    # `pg_stat_wal` is PostgreSQL 14+; on an older server the row says n/a.
    [ -n "$wal_before" ] || wal_before=n/a
    [ -n "$wal_after" ] || wal_after=n/a
    runs_delta=$(delta "$runs_before" "$runs_after")
    total_delta=$(delta "$total_before" "$total_after")
    row "pg_relation_size(${prefix}runs)" "$runs_before" "$runs_after" "$runs_delta"
    row "pg_total_relation_size(${prefix}body)" "$body_before" "$body_after" "$(delta "$body_before" "$body_after")"
    row "pg_relation_size(${prefix}journal)" "$journal_before" "$journal_after" "$(delta "$journal_before" "$journal_after")"
    row "pg_total_relation_size(${prefix}runs)" "$total_before" "$total_after" "$total_delta"
    row "pg_stat_wal.wal_bytes (PG 14+)" "$wal_before" "$wal_after" "$(delta "$wal_before" "$wal_after")"
    if [ "$runs_delta" = 0 ] && [ "$total_delta" = 0 ] && [ "$body_delta" = 0 ]; then
        echo "  warning: neither the runs relation, its total nor the body grew; check the table prefix and the database" >&2
    fi
    echo "  counting store: ${logical:-the workload printed no line}"
}

measure_mongo() {
    if [ -z "${ADAM_TEST_MONGODB_URI:-}" ]; then
        echo "mongo: skipped (ADAM_TEST_MONGODB_URI is not set)"
        return
    fi
    if ! mongo_eval 'print(1)' >/dev/null; then
        echo "mongo: FAILED (cannot query $(host_of "$ADAM_TEST_MONGODB_URI")/$mongo_db)" >&2
        fail=1
        return
    fi
    header "mongo $(host_of "$ADAM_TEST_MONGODB_URI")/$mongo_db, collections ${mongo_prefix}runs, ${mongo_prefix}body, ${mongo_prefix}journal"
    runs_before=$(mongo_storage "${mongo_prefix}runs")
    body_before=$(mongo_storage "${mongo_prefix}body")
    journal_before=$(mongo_storage "${mongo_prefix}journal")
    written_before=$(mongo_written)
    workload adam-store-mongodb
    runs_after=$(mongo_storage "${mongo_prefix}runs")
    body_after=$(mongo_storage "${mongo_prefix}body")
    journal_after=$(mongo_storage "${mongo_prefix}journal")
    written_after=$(mongo_written)
    runs_delta=$(delta "$runs_before" "$runs_after")
    body_delta=$(delta "$body_before" "$body_after")
    journal_delta=$(delta "$journal_before" "$journal_after")
    row "collStats(${mongo_prefix}runs).storageSize" "$runs_before" "$runs_after" "$runs_delta"
    row "collStats(${mongo_prefix}body).storageSize" "$body_before" "$body_after" "$body_delta"
    row "collStats(${mongo_prefix}journal).storageSize" "$journal_before" "$journal_after" "$journal_delta"
    bm_before=$(printf '%s' "$written_before" | cut -d' ' -f1)
    bm_after=$(printf '%s' "$written_after" | cut -d' ' -f1)
    log_before=$(printf '%s' "$written_before" | cut -d' ' -f2)
    log_after=$(printf '%s' "$written_after" | cut -d' ' -f2)
    row "wiredTiger block-manager bytes written" "$bm_before" "$bm_after" "$(delta "$bm_before" "$bm_after")"
    row "wiredTiger log bytes written" "$log_before" "$log_after" "$(delta "$log_before" "$log_after")"
    if [ "$runs_delta" = 0 ] && [ "$body_delta" = 0 ] && [ "$journal_delta" = 0 ]; then
        echo "  warning: no collection grew; check the collection prefix and the database" >&2
    fi
    echo "  counting store: ${logical:-the workload printed no line}"
}

echo "note: size deltas are retained bytes (a relation or collection includes bloat); pg_stat_wal.wal_bytes and the WiredTiger log are bytes written"

case $stores in
    postgres) measure_postgres ;;
    mongo) measure_mongo ;;
    both)
        measure_postgres
        measure_mongo
        ;;
esac

echo
if [ "$fail" -ne 0 ]; then
    echo "p2-measure: at least one store failed" >&2
fi
exit "$fail"
