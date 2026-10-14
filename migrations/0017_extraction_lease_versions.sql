-- Each leased thread's content version when it was leased, in thread_ids'
-- order: what submit_extracts records as extracted. A thread whose mail
-- changes during the lease stays changed, and is leased again. Leases from
-- before this column record the version at submission, as they did.

-- extraction_leases is locked against writes until the migration commits:
-- fail rather than wait behind a long writer, and bootstrap tries again.
SET LOCAL lock_timeout = '10s';

ALTER TABLE extraction_leases ADD COLUMN thread_versions int[];
