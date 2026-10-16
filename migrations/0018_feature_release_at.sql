-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT license.

-- When the evidence for a feature's release was written: the date of the
-- latest message of the thread whose extraction gave the release. A thread's
-- claim replaces the release only when its thread's latest message is at
-- least as recent, so that a thread of 2017 "targeting 11", extracted after
-- one of 2019 saying it shipped in 12, doesn't undo it, whatever the order
-- of extraction.
--
-- Nothing recorded which thread gave a release before, so a release already
-- set takes the date of its feature's most recent thread: the older claims
-- that the backfill now brings can't replace it.

-- features is locked against writes until the migration commits: fail
-- rather than wait behind a long writer, and bootstrap tries again.
SET LOCAL lock_timeout = '10s';

ALTER TABLE features ADD COLUMN release_at timestamptz;

UPDATE features f SET release_at = linked.last_at
FROM (SELECT ft.feature_id, max(t.last_at) AS last_at
      FROM feature_threads ft JOIN threads t ON t.id = ft.thread_id
      GROUP BY ft.feature_id) linked
WHERE linked.feature_id = f.id AND f.release IS NOT NULL;
