-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT license.

-- Mail ingestion. The subject key matches a reply without threading headers
-- (In-Reply-To, References) with its thread: the subject without reply
-- prefixes, list tags and patch tags, lowercased. The References index finds
-- the thread of messages that share an ancestor that isn't stored.

ALTER TABLE threads ADD COLUMN subject_key text NOT NULL DEFAULT '';
CREATE INDEX threads_subject_key ON threads (list, subject_key, last_at) WHERE subject_key <> '';
CREATE INDEX messages_references ON messages USING gin (references_ids);
