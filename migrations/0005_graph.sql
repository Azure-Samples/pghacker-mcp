-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT license.

-- Bookkeeping for projecting the tables into the AGE graph pghacker_graph:
-- which vertex and edge each table row became, so a sync changes only what
-- changed. The graph and its labels are created by bootstrap, as AGE requires.

CREATE TABLE graph_nodes (
    -- Such as message:<Message-ID>, author:<id>, file:<path>.
    key        text PRIMARY KEY,
    label      text NOT NULL,
    graph_id   ag_catalog.graphid NOT NULL UNIQUE,
    props      jsonb NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX graph_nodes_label ON graph_nodes (label);

CREATE TABLE graph_edges (
    label      text NOT NULL,
    start_key  text NOT NULL REFERENCES graph_nodes (key) ON DELETE CASCADE,
    end_key    text NOT NULL REFERENCES graph_nodes (key) ON DELETE CASCADE,
    graph_id   ag_catalog.graphid NOT NULL UNIQUE,
    props      jsonb NOT NULL DEFAULT '{}',
    PRIMARY KEY (label, start_key, end_key)
);

CREATE TABLE graph_sync_runs (
    id          bigserial PRIMARY KEY,
    started_at  timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz,
    stats       jsonb
);

REVOKE ALL ON graph_nodes, graph_edges, graph_sync_runs FROM pgh_reader;
