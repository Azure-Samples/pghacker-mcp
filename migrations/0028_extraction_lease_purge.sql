-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT license.

-- Drafts go with their thread. A draft's thread reference blocked deleting
-- the thread, so ingest didn't merge a thread that a draft named, as if an
-- extract cited it. A lease's drafts already go with the lease, and the
-- purge_leases job now deletes leases a day after they end, with their
-- drafts: nothing reads either once the lease ends.
--
-- The constraint replaces one that held for every row, so it skips checking
-- them again (NOT VALID), which would hold the locks while it reads drafts.

-- drafts and threads are locked against writes until the migration commits:
-- fail rather than wait behind a long writer, and bootstrap tries again.
SET LOCAL lock_timeout = '10s';

ALTER TABLE drafts
    DROP CONSTRAINT drafts_thread_id_fkey,
    ADD CONSTRAINT drafts_thread_id_fkey FOREIGN KEY (thread_id) REFERENCES threads (id) ON DELETE CASCADE NOT VALID;

COMMENT ON TABLE extraction_leases IS 'Threads leased to extractors by next_extraction_batch: no other extractor gets a thread while its lease is active. The purge_leases job deletes leases a day after they were released or expired, with their drafts.';
COMMENT ON COLUMN drafts.thread_id IS 'The thread it comes from (threads.id); deleted with it, as when ingest merges the thread into another.';
