-- The schema version 10 that adam-store-postgres emits.
-- Regenerate with ADAM_WRITE_FIXTURE=1 cargo test -p adam-store-postgres --lib the_statements_equal_the_version_10_fixture
CREATE TABLE IF NOT EXISTS adam_meta (
                    key   TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );

CREATE TABLE IF NOT EXISTS adam_runs (
                    id              UUID        NOT NULL,
                    agent           TEXT        NOT NULL,
                    conversation_id TEXT,
                    parent_id       UUID,
                    status          TEXT        NOT NULL CHECK (status IN ('runnable', 'parked', 'done', 'failed')),
                    state           JSONB       NOT NULL,
                    wake_at         TIMESTAMPTZ,
                    sched_at        TIMESTAMPTZ,
                    version         BIGINT      NOT NULL CHECK (version > 0),
                    lease_owner     TEXT,
                    lease_until     TIMESTAMPTZ,
                    owner           TEXT,
                    head            JSONB,
                    head_version    BIGINT,
                    created_at      TIMESTAMPTZ NOT NULL,
                    updated_at      TIMESTAMPTZ NOT NULL,
                    CONSTRAINT adam_runs_pkey PRIMARY KEY (id)
                );

CREATE TABLE IF NOT EXISTS adam_push (
                    run_id          UUID        NOT NULL REFERENCES adam_runs (id) ON DELETE CASCADE,
                    id              TEXT        NOT NULL,
                    agent           TEXT        NOT NULL,
                    owner           TEXT        NOT NULL,
                    config          JSONB       NOT NULL,
                    cursor          JSONB       NOT NULL,
                    state           TEXT        NOT NULL CHECK (state IN ('active', 'done', 'gave_up')),
                    attempts        INTEGER     NOT NULL CHECK (attempts >= 0),
                    last_error      TEXT,
                    next_attempt_at TIMESTAMPTZ NOT NULL,
                    version         BIGINT      NOT NULL CHECK (version > 0),
                    lease_owner     TEXT,
                    lease_until     TIMESTAMPTZ,
                    created_at      TIMESTAMPTZ NOT NULL,
                    updated_at      TIMESTAMPTZ NOT NULL,
                    PRIMARY KEY (run_id, id)
                );

CREATE INDEX IF NOT EXISTS adam_push_due ON adam_push (agent, next_attempt_at, run_id, id)
                 WHERE state = 'active';

ALTER TABLE adam_runs ADD COLUMN IF NOT EXISTS owner TEXT;

CREATE INDEX IF NOT EXISTS adam_runs_due ON adam_runs (agent, sched_at, id)
                 WHERE sched_at IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS adam_runs_open_conversation
                 ON adam_runs (agent, conversation_id)
                 WHERE conversation_id IS NOT NULL AND status IN ('runnable', 'parked');

CREATE INDEX IF NOT EXISTS adam_runs_finished ON adam_runs (agent, updated_at)
                 WHERE status IN ('done', 'failed');

CREATE INDEX IF NOT EXISTS adam_runs_parent ON adam_runs (parent_id) WHERE parent_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS adam_runs_list_status
                 ON adam_runs (agent, (conversation_id COLLATE "C"), updated_at DESC, id DESC)
                 INCLUDE (status)
                 WHERE conversation_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS adam_journal (
                    run_id      UUID        NOT NULL REFERENCES adam_runs (id) ON DELETE CASCADE,
                    seq         BIGINT      NOT NULL CHECK (seq >= 0),
                    name        TEXT        NOT NULL,
                    ok          BOOLEAN     NOT NULL,
                    payload     JSONB       NOT NULL,
                    recorded_at TIMESTAMPTZ NOT NULL,
                    PRIMARY KEY (run_id, seq)
                );

CREATE TABLE IF NOT EXISTS adam_blob (
                    digest      TEXT        NOT NULL CHECK (digest ~ '^[0-9a-f]{64}$'),
                    bytes       BYTEA       NOT NULL,
                    size        BIGINT      NOT NULL CHECK (size = octet_length(bytes)),
                    created_at  TIMESTAMPTZ NOT NULL,
                    CONSTRAINT adam_blob_pkey PRIMARY KEY (digest)
                );

CREATE INDEX IF NOT EXISTS adam_blob_gc ON adam_blob (created_at);

CREATE TABLE IF NOT EXISTS adam_run_blob (
                    run_id  UUID NOT NULL REFERENCES adam_runs (id) ON DELETE CASCADE,
                    digest  TEXT NOT NULL,
                    CONSTRAINT adam_run_blob_pkey PRIMARY KEY (run_id, digest)
                );

CREATE INDEX IF NOT EXISTS adam_run_blob_digest ON adam_run_blob (digest);

ALTER TABLE adam_runs ADD COLUMN IF NOT EXISTS head JSONB;

ALTER TABLE adam_runs ADD COLUMN IF NOT EXISTS head_version BIGINT;

CREATE TABLE IF NOT EXISTS adam_body (
                    run_id      UUID        NOT NULL REFERENCES adam_runs (id) ON DELETE CASCADE,
                    generation  UUID        NOT NULL,
                    idx         INTEGER     NOT NULL CHECK (idx >= 0),
                    message     JSONB       NOT NULL,
                    writer      UUID        NOT NULL,
                    written_at  TIMESTAMPTZ NOT NULL,
                    PRIMARY KEY (run_id, generation, idx)
                );

CREATE INDEX IF NOT EXISTS adam_body_written ON adam_body (written_at);

ALTER TABLE adam_runs ADD COLUMN IF NOT EXISTS served JSONB
                 GENERATED ALWAYS AS (CASE WHEN jsonb_typeof(head) = 'object' AND (head_version = version OR state = 'null'::jsonb) THEN head ELSE state END) VIRTUAL;

CREATE OR REPLACE FUNCTION adam_claim_due(
             in_agents text[], in_worker text, in_pinned boolean, in_busy uuid[],
             in_now timestamptz, in_until timestamptz, in_limit bigint)
         RETURNS TABLE (id uuid, agent text, conversation_id text, parent_id uuid, status text,
                        served jsonb, wake_at timestamptz, version bigint,
                        created_at timestamptz, updated_at timestamptz)
         LANGUAGE sql
         BEGIN ATOMIC
             WITH due AS (
                 SELECT d.id FROM adam_runs d
                  WHERE d.agent = ANY(in_agents)
                    AND d.id <> ALL(in_busy)
                    AND d.sched_at <= in_now
                    AND (d.lease_until IS NULL OR d.lease_until <= in_now)
                    AND (NOT in_pinned OR d.owner IS NULL OR d.owner = in_worker)
                  ORDER BY d.sched_at, d.id
                  LIMIT in_limit
                  FOR UPDATE SKIP LOCKED
             ), claimed AS (
                 UPDATE adam_runs r
                    SET lease_owner = in_worker, lease_until = in_until,
                        owner = CASE WHEN in_pinned THEN COALESCE(r.owner, in_worker) ELSE r.owner END
                   FROM due
                  WHERE r.id = due.id
                 RETURNING r.id, r.agent, r.conversation_id, r.parent_id, r.status, r.served,
                           r.wake_at, r.version, r.created_at, r.updated_at, r.sched_at
             )
             SELECT c.id, c.agent, c.conversation_id, c.parent_id, c.status, c.served,
                    c.wake_at, c.version, c.created_at, c.updated_at
               FROM claimed c
              ORDER BY c.sched_at, c.id;
         END;

CREATE TABLE IF NOT EXISTS adam_memory_lesson (
                scope      TEXT        NOT NULL,
                audience   TEXT        NOT NULL,
                root_cause TEXT        NOT NULL COLLATE "C",
                remedy     TEXT        NOT NULL,
                rationale  TEXT        NOT NULL,
                anchors    JSONB       NOT NULL,
                sources    JSONB       NOT NULL,
                review     TEXT        NOT NULL,
                state      TEXT        NOT NULL,
                created    TIMESTAMPTZ NOT NULL,
                updated    TIMESTAMPTZ NOT NULL,
                retired_at TIMESTAMPTZ,
                CONSTRAINT adam_memory_lesson_pkey PRIMARY KEY (scope, audience, root_cause),
                CONSTRAINT adam_memory_lesson_state CHECK (state IN ('proposed', 'active', 'retired'))
            );

CREATE INDEX IF NOT EXISTS adam_memory_lesson_active ON adam_memory_lesson (scope, audience, updated DESC, root_cause)
             WHERE state = 'active';

INSERT INTO adam_meta (key, value) VALUES ('schema_version', '10')
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value
              WHERE adam_meta.value::INT < EXCLUDED.value::INT;
