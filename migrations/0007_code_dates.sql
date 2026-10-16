-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT license.

-- When each snapshot's commit was committed: the snapshot shows its branch,
-- and so its release, as of that moment.

ALTER TABLE code_snapshots ADD COLUMN committed_at timestamptz;
