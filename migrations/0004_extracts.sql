-- Extracts that crawler sessions submit: features, decisions and positions,
-- each citing messages with verbatim quotes; and the leases and drafts of the
-- extraction workflow.

CREATE TABLE features (
    id          bigserial PRIMARY KEY,
    slug        text NOT NULL UNIQUE,
    name        text NOT NULL,
    description text NOT NULL,
    status      text NOT NULL CHECK (status IN (
                    'proposed', 'in_progress', 'committed', 'reverted', 'rejected',
                    'returned', 'withdrawn', 'unknown')),
    -- The major version it targeted or shipped in, such as '19'.
    release     text,
    merged_into bigint REFERENCES features (id),
    embedding   halfvec(1536),
    search_text text GENERATED ALWAYS AS (name || E'\n' || description) STORED,
    created_by  text,
    model       text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX features_bm25 ON features USING bm25 (search_text) WITH (text_config = 'english');
CREATE INDEX features_embedding ON features USING hnsw (embedding halfvec_cosine_ops);
CREATE INDEX features_name_trgm ON features USING gin (name gin_trgm_ops);

CREATE TABLE decisions (
    id           bigserial PRIMARY KEY,
    feature_id   bigint REFERENCES features (id),
    thread_id    bigint NOT NULL REFERENCES threads (id),
    title        text NOT NULL,
    outcome      text NOT NULL,
    alternatives text NOT NULL DEFAULT '',
    rationale    text NOT NULL,
    decided_at   timestamptz,
    merged_into  bigint REFERENCES decisions (id),
    embedding    halfvec(1536),
    search_text  text GENERATED ALWAYS AS (
                     title || E'\n' || outcome || E'\n' || rationale || E'\n' || alternatives) STORED,
    created_by   text,
    model        text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX decisions_bm25 ON decisions USING bm25 (search_text) WITH (text_config = 'english');
CREATE INDEX decisions_embedding ON decisions USING hnsw (embedding halfvec_cosine_ops);
CREATE INDEX decisions_feature ON decisions (feature_id);

CREATE TABLE positions (
    id          bigserial PRIMARY KEY,
    author_id   bigint NOT NULL REFERENCES authors (id),
    thread_id   bigint NOT NULL REFERENCES threads (id),
    feature_id  bigint REFERENCES features (id),
    decision_id bigint REFERENCES decisions (id),
    stance      text NOT NULL CHECK (stance IN (
                    'proposes', 'supports', 'opposes', 'concerned', 'neutral', 'reviews')),
    summary     text NOT NULL,
    embedding   halfvec(1536),
    search_text text GENERATED ALWAYS AS (summary) STORED,
    created_by  text,
    model       text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    CHECK (feature_id IS NOT NULL OR decision_id IS NOT NULL)
);
CREATE INDEX positions_bm25 ON positions USING bm25 (search_text) WITH (text_config = 'english');
CREATE INDEX positions_embedding ON positions USING hnsw (embedding halfvec_cosine_ops);
CREATE INDEX positions_author ON positions (author_id);
CREATE INDEX positions_thread ON positions (thread_id);

CREATE TABLE citations (
    id           bigserial PRIMARY KEY,
    extract_kind text NOT NULL CHECK (extract_kind IN ('feature', 'decision', 'position')),
    extract_id   bigint NOT NULL,
    message_id   bigint NOT NULL REFERENCES messages (id),
    quote        text NOT NULL,
    UNIQUE (extract_kind, extract_id, message_id, quote)
);
CREATE INDEX citations_extract ON citations (extract_kind, extract_id);
CREATE INDEX citations_message ON citations (message_id);

-- DISCUSSES: Thread -> Feature.
CREATE TABLE feature_threads (
    feature_id bigint NOT NULL REFERENCES features (id),
    thread_id  bigint NOT NULL REFERENCES threads (id),
    PRIMARY KEY (feature_id, thread_id)
);
CREATE INDEX feature_threads_thread ON feature_threads (thread_id);

-- IMPLEMENTS: File -> Feature, with the thread whose patches showed it.
CREATE TABLE feature_files (
    feature_id bigint NOT NULL REFERENCES features (id),
    path       text NOT NULL,
    thread_id  bigint REFERENCES threads (id),
    PRIMARY KEY (feature_id, path)
);

CREATE TABLE extraction_leases (
    token       uuid PRIMARY KEY,
    thread_ids  bigint[] NOT NULL,
    session     text,
    model       text,
    leased_at   timestamptz NOT NULL DEFAULT now(),
    expires_at  timestamptz NOT NULL,
    released_at timestamptz
);
CREATE INDEX extraction_leases_active ON extraction_leases USING gin (thread_ids) WHERE released_at IS NULL;

-- Draft features and decisions, with the existing extracts they resemble:
-- submit_extracts accepts only drafts that went through match_extracts.
CREATE TABLE drafts (
    id          bigserial PRIMARY KEY,
    lease_token uuid NOT NULL REFERENCES extraction_leases (token) ON DELETE CASCADE,
    thread_id   bigint NOT NULL REFERENCES threads (id),
    kind        text NOT NULL CHECK (kind IN ('feature', 'decision')),
    draft       jsonb NOT NULL,
    embedding   halfvec(1536),
    candidates  jsonb NOT NULL DEFAULT '[]',
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX drafts_lease ON drafts (lease_token);

REVOKE ALL ON extraction_leases, drafts FROM pgh_reader;
