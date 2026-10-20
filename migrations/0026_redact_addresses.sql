-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT license.

-- Mail is stored with its email addresses redacted: ingest replaces each in
-- subjects, bodies and patches with [email], and senders' names lose theirs
-- (pghacker-mail's redact module). This queues the redact_addresses job,
-- once (key redact), which redacts the mail stored before, a batch of
-- messages at a time; then authors' names and the extracts' text and quotes,
-- clearing changed extracts' embeddings and giving the threads they come
-- from a new content version, to be extracted again; then the patch_texts
-- lines with addresses that no patch uses any more. Changed chunks are
-- embedded again and changed patches indexed again by the recurring jobs, and
-- the graph follows the changed rows. author_emails keeps addresses, to match
-- senders to authors; pghacker-mcp can't read it.

INSERT INTO jobs (kind, key, priority)
SELECT 'redact_addresses', 'redact', 1 WHERE EXISTS (SELECT FROM messages)
ON CONFLICT (kind, key) DO UPDATE
SET state = 'queued', payload = '{}', attempts = 0, run_after = now(), last_error = NULL,
    result = NULL, finished_at = NULL, locked_by = NULL, locked_at = NULL;

COMMENT ON COLUMN messages.from_name IS 'The From display name, without email addresses; the address''s local part if it gives no name.';
COMMENT ON COLUMN messages.subject IS 'Its Subject header, decoded, with email addresses replaced by [email].';
COMMENT ON COLUMN messages.body IS 'Its text/plain body, decoded, with email addresses replaced by [email]; Message-IDs in URLs are kept.';
COMMENT ON COLUMN messages.body_clean IS 'The body without quoted replies, signatures and inline patches, addresses redacted as in body: what search indexes.';
COMMENT ON COLUMN citations.quote IS 'Text quoted verbatim from the message as stored, addresses redacted.';
