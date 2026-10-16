-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT license.

-- Pairs of features or decisions judged to be different things: by an
-- extractor that created one despite the other (its distinct_because), or by
-- the deduplicator (mark_distinct). find_duplicates doesn't offer them again.

CREATE TABLE distinct_pairs (
    kind       text NOT NULL CHECK (kind IN ('feature', 'decision')),
    low_id     bigint NOT NULL,
    high_id    bigint NOT NULL,
    reason     text NOT NULL,
    judged_by  text NOT NULL CHECK (judged_by IN ('extractor', 'dedup')),
    session    text,
    model      text,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (kind, low_id, high_id),
    CHECK (low_id < high_id)
);
REVOKE ALL ON distinct_pairs FROM pgh_reader;
