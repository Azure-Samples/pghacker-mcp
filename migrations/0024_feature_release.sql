-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT license.

-- A feature's release is one major version: the one it shipped in, or
-- targets until it's committed, never a branch it was back-ported to. Each
-- extraction that gives one replaces it, even from an older thread, and
-- submit_extracts saves no other value. 0023's comment had it come from the
-- feature's thread with the latest message.
--
-- COMMENT ON takes a SHARE UPDATE EXCLUSIVE lock on features until the
-- migration commits: it waits for a vacuum or ANALYZE of the table, never for
-- reads or writes. Past the lock timeout it fails, and bootstrap tries again.

COMMENT ON COLUMN features.release IS 'The one major version it shipped in, or targets until it''s committed, by the name it shipped as: 9.6 before 10, 17 from 10 on, 9.0 not its development name 8.5. Never a branch it was back-ported to. Each extraction that gives one replaces it.';
