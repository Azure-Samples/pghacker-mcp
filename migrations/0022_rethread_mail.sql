-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT license.

-- Threading stored mail again (crates/pghacker-indexer/src/rethread.rs).
-- Old mailers wrote senders' addresses and placeholders into In-Reply-To
-- and References (exmh's `Message from Tom Lane <tgl@…> of "…" <id>`, elm's
-- `<no.id>`), and replies on topics of their own shared their parents'
-- threads: ingest now threads such mail apart, and this queues the job that
-- threads the stored mail again. From here on, messages store only the IDs
-- that name messages in parent_message_id and references_ids.
--
-- As pgh_indexer, SELECT pghacker.rethread_mail(); queues a dry run, whose
-- report the job records (SELECT result FROM pghacker.jobs WHERE kind =
-- 'rethread_mail' AND key = 'dry-run'); SELECT pghacker.rethread_mail('<plan>');
-- applies the groups of that report's plan that are still as it found them.
-- A job that's queued or running is left as it is.
CREATE FUNCTION rethread_mail(plan text DEFAULT NULL) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
    job_key text := CASE WHEN plan IS NULL THEN 'dry-run' ELSE 'apply' END;
    queued bigint;
BEGIN
    INSERT INTO pghacker.jobs (kind, key, payload, priority)
    VALUES ('rethread_mail', job_key, jsonb_build_object('plan', plan), 1)
    ON CONFLICT (kind, key) DO UPDATE
        SET state = 'queued', run_after = now(), attempts = 0, last_error = NULL, result = NULL,
            payload = EXCLUDED.payload, finished_at = NULL
        WHERE jobs.state IN ('done', 'failed', 'cancelled')
    RETURNING id INTO queued;
    IF queued IS NULL THEN
        RETURN format('the %s is already queued or running', job_key);
    END IF;
    RETURN format('queued the %s: its report is SELECT result FROM pghacker.jobs '
                  'WHERE kind = ''rethread_mail'' AND key = %L', job_key, job_key);
END
$$;
