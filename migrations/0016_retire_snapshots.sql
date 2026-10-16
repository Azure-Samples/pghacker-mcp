-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT license.

-- The code tools read the commit history (0011) alone, so the snapshots it
-- replaced are retired: their tables, the capture of their changes for the
-- graph, and their loader's unfinished jobs.
--
-- The graph's File and Subsystem vertices and PART_OF edges now come from the
-- history's file versions, which have no triggers. The keys that either the
-- snapshots or the history give are recorded first, so the incremental graph
-- sync recomputes just those from the new source, without a full sync.

-- Dropping the tables waits for their readers: fail instead of stalling the
-- other tables' writers behind it, and bootstrap tries again.
SET LOCAL lock_timeout = '10s';

INSERT INTO graph_changes (label, key, end_key)
SELECT DISTINCT e.*
FROM (
    SELECT f.path, f.subsystem
    FROM code_files f JOIN code_snapshots s ON s.id = f.snapshot_id
    WHERE s.state = 'ready' AND s.repo = 'postgres'
    UNION
    SELECT v.path, v.subsystem FROM code_file_versions v
) x
CROSS JOIN LATERAL graph_code_file_entries(x.path, x.subsystem) e;

DROP TABLE code_lines, code_files, code_snapshots;

-- graph_code_file_entries stays: it describes any file's graph entries.
DROP FUNCTION graph_capture_code_files(), graph_capture_code_snapshots();

UPDATE jobs SET state = 'cancelled', finished_at = now()
WHERE kind = 'code_snapshot' AND state IN ('queued', 'running');
