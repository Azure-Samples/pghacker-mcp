-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT license.

-- restore_addresses() (0015) queued the jobs that put email addresses back
-- into mail stored without them. The indexer no longer runs those jobs: the
-- function goes, and unfinished ones are cancelled, which the indexer would
-- otherwise fail as a kind it doesn't know. Finished ones stay in jobs, as
-- history. Neither statement waits for readers or writers: the drop locks
-- the function, and the update the rows it cancels.
DROP FUNCTION restore_addresses();

UPDATE jobs SET state = 'cancelled', finished_at = now()
WHERE kind = 'restore_addresses' AND state IN ('queued', 'running');
