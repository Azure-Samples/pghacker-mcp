-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT license.

-- A feature's status follows its threads' evidence, whatever the order of
-- extraction. Each thread's link to the feature records the status the
-- thread showed and when the thread's latest message was written then
-- (feature_threads.status, status_at); the feature's status folds that
-- evidence, with that of the features merged into it, in date order: each
-- replaces the status so far, except that a proposal or work in progress
-- never replaces a commit. Only a more advanced status replaced it before,
-- so a reverted feature stayed reverted after it was committed again, and a
-- rejected one stayed rejected once revived.
--
-- Nothing recorded which thread gave a status before: a status already set
-- becomes the evidence of its feature's most recent thread, other than those
-- a merge linked it to, as release_at's date did (0018). The older threads
-- that the backfill brings can't replace it, and newer ones can.

-- features and feature_threads are locked against writes until the
-- migration commits: fail rather than wait behind a long writer, and
-- bootstrap tries again.
SET LOCAL lock_timeout = '10s';

ALTER TABLE features ADD COLUMN status_at timestamptz;
ALTER TABLE feature_threads
    ADD COLUMN status text CHECK (status IN ('proposed', 'in_progress', 'committed', 'reverted', 'rejected',
                                             'returned', 'withdrawn')),
    ADD COLUMN status_at timestamptz;

UPDATE feature_threads ft SET status = f.status, status_at = t.last_at
FROM features f, threads t
WHERE f.id = ft.feature_id AND t.id = ft.thread_id AND f.status <> 'unknown'
  AND ft.thread_id = (
      SELECT o.thread_id FROM feature_threads o JOIN threads ot ON ot.id = o.thread_id
      WHERE o.feature_id = f.id
      ORDER BY EXISTS (SELECT FROM feature_threads m JOIN features mf ON mf.id = m.feature_id
                       WHERE mf.merged_into = f.id AND m.thread_id = o.thread_id),
               ot.last_at DESC NULLS LAST, o.thread_id
      LIMIT 1);

UPDATE features f SET status_at = ft.status_at
FROM feature_threads ft
WHERE ft.feature_id = f.id AND ft.status IS NOT NULL;

COMMENT ON COLUMN features.status IS 'What its threads show, by their evidence in date order (feature_threads.status, with that of the features merged into it): proposed, in_progress, committed, reverted, rejected, returned (with feedback, in a CommitFest), withdrawn or unknown. Newer evidence replaces older, except that proposed or in_progress never replaces committed; on the same date the more advanced wins.';
COMMENT ON COLUMN features.status_at IS 'When the evidence for its status was written: the latest message of the thread that gave it, then; NULL while the status is unknown or undated.';
COMMENT ON COLUMN feature_threads.status IS 'The status the thread showed the feature with when last extracted (combined with what it showed before, as features.status combines threads); NULL if it showed none.';
COMMENT ON COLUMN feature_threads.status_at IS 'When the thread''s latest message was written when it showed that status.';
