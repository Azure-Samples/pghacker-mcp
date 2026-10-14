-- Mail stored before ingest kept email addresses has `[email]` in their
-- place, and senders' names without them. Ingest keeps no raw mail, so the
-- mail is retrieved again (crates/pghacker-indexer/src/addresses.rs). This
-- queues the jobs that do it: one per stored month of each list, which reads
-- the month again, and one per 200 messages from thread pages, which are
-- retrieved raw. A job queued or running is left as it is; a finished one is
-- queued again. As pgh_indexer: SELECT * FROM pghacker.restore_addresses();
-- or pghacker-indexer restore-addresses.
CREATE FUNCTION restore_addresses(OUT months bigint, OUT batches bigint, OUT messages bigint)
LANGUAGE plpgsql AS $$
BEGIN
    -- A stripped sender's name leaves no mark, so every month is read again.
    WITH queued AS (
        INSERT INTO pghacker.jobs (source_id, kind, key, payload, priority)
        SELECT min(m.source_id), 'restore_addresses', m.list || '/' || m.archive_month,
               jsonb_build_object('list', m.list, 'month', m.archive_month), 1
        FROM pghacker.messages m WHERE m.archive_month IS NOT NULL
        GROUP BY m.list, m.archive_month
        ON CONFLICT (kind, key) DO UPDATE
            SET state = 'queued', run_after = now(), attempts = 0, last_error = NULL,
                payload = EXCLUDED.payload, source_id = EXCLUDED.source_id, finished_at = NULL
            WHERE jobs.state IN ('done', 'failed', 'cancelled')
        RETURNING 1
    )
    SELECT count(*) INTO months FROM queued;

    WITH pages AS (
        SELECT m.id, m.message_id, (row_number() OVER (ORDER BY m.id) - 1) / 200 AS batch
        FROM pghacker.messages m WHERE m.archive_month IS NULL
    ), grouped AS (
        SELECT min(id) AS first, array_agg(message_id ORDER BY id) AS ids FROM pages GROUP BY batch
    ), queued AS (
        INSERT INTO pghacker.jobs (kind, key, payload, priority)
        SELECT 'restore_addresses', 'thread-pages/' || first, jsonb_build_object('message_ids', to_jsonb(ids)), 1
        FROM grouped
        ON CONFLICT (kind, key) DO UPDATE
            SET state = 'queued', run_after = now(), attempts = 0, last_error = NULL,
                payload = EXCLUDED.payload, source_id = EXCLUDED.source_id, finished_at = NULL
            WHERE jobs.state IN ('done', 'failed', 'cancelled')
        RETURNING jsonb_array_length(payload -> 'message_ids') AS n
    )
    SELECT count(*), coalesce(sum(n), 0) INTO batches, messages FROM queued;
END
$$;
