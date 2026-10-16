-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT license.

-- Patches' series versions become text: the version as the file name or the
-- subject writes it (3, 3.1, 6a, 01, 20261003, 12-pg13), by rules checked
-- against every patch name stored (crates/pghacker-mail/src/series.rs).
-- Parts stay numbers, from file names only.
--
-- The column is replaced rather than converted, which would rewrite the
-- table and its indexes under an exclusive lock. The patch_series job parses
-- the names and subjects again, filling the new column and correcting parts;
-- versions read empty until it has. Images before this migration read the
-- column as a number: rolling back to one needs a number column back
-- (deploy/README.md).

-- Replacing the column waits for patches' readers and writers: fail rather
-- than stall them behind it, and bootstrap tries again.
SET LOCAL lock_timeout = '10s';

ALTER TABLE patches DROP COLUMN series_version;
ALTER TABLE patches ADD COLUMN series_version text;

INSERT INTO jobs (kind, key)
SELECT 'patch_series', 'reparse' WHERE EXISTS (SELECT FROM patches)
ON CONFLICT (kind, key) DO UPDATE
SET state = 'queued', payload = '{}', attempts = 0, run_after = now(), last_error = NULL,
    result = NULL, finished_at = NULL, locked_by = NULL, locked_at = NULL;
