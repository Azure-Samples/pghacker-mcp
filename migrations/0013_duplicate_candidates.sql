-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT license.

-- Duplicate candidates for find_duplicates: pairs of features, or of
-- decisions, that look alike, found as each extract arrives rather than by
-- comparing every pair at once. The duplicates job compares each new extract
-- with the live ones, by its embedding's nearest neighbours and by its name's
-- trigrams, and keeps the pairs that come close; find_duplicates reads them,
-- leaving out merged extracts and pairs judged different (distinct_pairs).

-- The index locks decisions against writes until the migration commits: fail
-- rather than wait behind a long writer, and bootstrap tries again.
SET LOCAL lock_timeout = '10s';

CREATE TABLE duplicate_candidates (
    kind            text NOT NULL CHECK (kind IN ('feature', 'decision')),
    low_id          bigint NOT NULL,
    high_id         bigint NOT NULL,
    -- Cosine similarity of the embeddings; NULL while either has none.
    semantic        float8,
    -- Trigram similarity of the names (a decision's title).
    name_similarity float8 NOT NULL,
    found_at        timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (kind, low_id, high_id),
    CHECK (low_id < high_id)
);

-- The extracts already compared with the others, and whether by embedding
-- too: one compared before its embedding came is compared again.
CREATE TABLE duplicate_checks (
    kind       text NOT NULL CHECK (kind IN ('feature', 'decision')),
    extract_id bigint NOT NULL,
    semantic   boolean NOT NULL,
    checked_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (kind, extract_id)
);

-- Decisions' titles by trigram, as features_name_trgm has features' names.
CREATE INDEX decisions_title_trgm ON decisions USING gin (title gin_trgm_ops);

REVOKE ALL ON duplicate_candidates, duplicate_checks FROM pgh_reader;
