-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT license.

-- A feature's release by the name it shipped as. Four major releases were
-- renamed during their development, and their threads name them by their
-- development names, which extraction saved as written:
--   6.6 shipped as 7.0 (87af721, "Update version for 7.0"),
--   7.5 as 8.0 (fcbc438, "Label CVS tip as 8.0devel instead of 7.5devel"),
--   8.5 as 9.0 (50a90fa, "this release will be 9.0 not 8.5", after the
--   alphas tagged REL8_5_ALPHA1 to 3),
--   9.7 as 10 (never stamped: ca9112a stamped 10devel after 9.6).
-- Every other development name shipped as it was.
--
-- submit_extracts saves a release through shipped_release(), and the
-- features saved before take their shipped names here. The change capture
-- on features (0010) moves their TARGETS edges, and the development names'
-- Release vertices go. Images before this migration save names as written:
-- deploy/README.md says how to map them again.

-- features is locked against writes until the migration commits: fail
-- rather than wait behind a long writer, and bootstrap tries again.
SET LOCAL lock_timeout = '10s';

CREATE FUNCTION shipped_release(p_release text) RETURNS text
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT CASE p_release WHEN '6.6' THEN '7.0' WHEN '7.5' THEN '8.0'
                          WHEN '8.5' THEN '9.0' WHEN '9.7' THEN '10' ELSE p_release END
$$;

UPDATE features SET release = shipped_release(release)
WHERE release IS DISTINCT FROM shipped_release(release);
