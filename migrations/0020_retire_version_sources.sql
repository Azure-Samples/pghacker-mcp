-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT license.

-- PostgreSQL's code isn't a source. The commit history (0011) holds every
-- branch, tag and commit of its git repository: an operator builds it, the
-- code_history_sync job applies new commits, and code_history records where
-- it comes from and when it last synced. Since the snapshots were retired
-- (0016), add_source has stored no version, but the versions their loader
-- stored remained, with its jobs. They go, and sources holds mailing lists
-- only.

-- sources is locked until the migration commits: fail rather than wait
-- behind a long ingest, and bootstrap tries again.
SET LOCAL lock_timeout = '10s';

-- Their code_snapshot jobs go with them (ON DELETE CASCADE), and no message
-- names one.
DELETE FROM sources WHERE kind = 'postgres_version';
DELETE FROM jobs WHERE kind = 'code_snapshot';

ALTER TABLE sources DROP CONSTRAINT sources_kind_check,
    ADD CONSTRAINT sources_kind_check CHECK (kind IN ('mailing_list'));

COMMENT ON TABLE sources IS 'The mailing lists the indexer retrieves; config {"follow": true} retrieves a list''s current month again every hour. PostgreSQL''s code is not a source: code_history describes the commit history that the code tools read, its repository and its last sync.';
