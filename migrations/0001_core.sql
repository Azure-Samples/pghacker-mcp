-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT license.

-- Sources the indexer retrieves, the jobs that retrieve and process them, and
-- the log of reversible merges. Runs as pgh_indexer with search_path pghacker.

CREATE TABLE sources (
    id          bigserial PRIMARY KEY,
    kind        text NOT NULL CHECK (kind IN ('mailing_list', 'postgres_version')),
    name        text NOT NULL,
    -- mailing_list: {} (the configured slice decides what to retrieve);
    -- postgres_version: {"repo_url", "branch", "sha", "paths": [...]}.
    config      jsonb NOT NULL DEFAULT '{}',
    enabled     boolean NOT NULL DEFAULT true,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (kind, name)
);

CREATE TABLE jobs (
    id           bigserial PRIMARY KEY,
    source_id    bigint REFERENCES sources (id) ON DELETE CASCADE,
    kind         text NOT NULL,
    -- Idempotency: one job per (kind, key), such as ('mbox_month', 'pgsql-hackers/2026-06').
    key          text NOT NULL,
    payload      jsonb NOT NULL DEFAULT '{}',
    state        text NOT NULL DEFAULT 'queued'
                 CHECK (state IN ('queued', 'running', 'done', 'failed', 'cancelled')),
    priority     int NOT NULL DEFAULT 0,
    attempts     int NOT NULL DEFAULT 0,
    max_attempts int NOT NULL DEFAULT 5,
    run_after    timestamptz NOT NULL DEFAULT now(),
    locked_by    text,
    locked_at    timestamptz,
    last_error   text,
    result       jsonb,
    created_at   timestamptz NOT NULL DEFAULT now(),
    finished_at  timestamptz,
    UNIQUE (kind, key)
);
CREATE INDEX jobs_ready ON jobs (priority DESC, run_after, id) WHERE state = 'queued';
CREATE INDEX jobs_source ON jobs (source_id, kind, state);

CREATE TABLE merge_log (
    id          bigserial PRIMARY KEY,
    kind        text NOT NULL CHECK (kind IN ('author', 'feature', 'decision')),
    from_id     bigint NOT NULL,
    into_id     bigint NOT NULL,
    -- What the merge re-pointed, so that it can be undone.
    moved       jsonb NOT NULL,
    reason      text NOT NULL,
    actor       text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    reverted_at timestamptz
);

-- Internal: the reader role sees neither the job queue nor the merge log.
REVOKE ALL ON sources, jobs, merge_log FROM pgh_reader;
GRANT SELECT ON sources TO pgh_reader;
