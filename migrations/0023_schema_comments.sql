-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT license.

-- Descriptions of every table and column of schema pghacker, as comments:
-- obj_description and col_description read them, as get_ontology tells the
-- agents, which read them as pgh_reader. sources keeps its comment (0020).
-- A history build copies the comments of the history's tables (code_*) to the
-- tables it builds and installs (crates/pghacker-code/src/history/store.rs).
--
-- COMMENT ON takes a SHARE UPDATE EXCLUSIVE lock on each table until the
-- migration commits: it waits for a vacuum or ANALYZE of the table, never for
-- reads or writes. Past the lock timeout it fails, and bootstrap tries again.
-- The history's tables come in the order a history install locks them.

-- Mail.

COMMENT ON COLUMN sources.id IS 'The source''s ID, which messages.source_id and jobs.source_id name.';
COMMENT ON COLUMN sources.kind IS 'Always mailing_list, the only kind of source.';
COMMENT ON COLUMN sources.name IS 'The mailing list''s name in the postgresql.org archive, such as pgsql-hackers, as messages.list and threads.list name it.';
COMMENT ON COLUMN sources.config IS '{"follow": true} retrieves the list''s current month again every hour; {} or {"follow": false} doesn''t.';
COMMENT ON COLUMN sources.enabled IS 'False once remove_source stopped retrieving the list.';
COMMENT ON COLUMN sources.created_at IS 'When the list was first added.';
COMMENT ON COLUMN sources.updated_at IS 'When the list was last added again or removed.';

COMMENT ON TABLE authors IS 'People who sent mail: matched by their address (author_emails), or without one by their name; merge_authors folds duplicates into one.';
COMMENT ON COLUMN authors.id IS 'The author''s ID, which messages.author_id and positions.author_id name; the graph keys its vertex author:<id>.';
COMMENT ON COLUMN authors.display_name IS 'The name the author sent the most mail under (author_names).';
COMMENT ON COLUMN authors.merged_into IS 'The author this one was merged into (authors.id), which now has its messages and positions; NULL unless merged.';
COMMENT ON COLUMN authors.message_count IS 'Messages the author sent; 0 once merged.';
COMMENT ON COLUMN authors.first_seen IS 'When the author''s earliest message was sent.';
COMMENT ON COLUMN authors.last_seen IS 'When the author''s latest message was sent.';
COMMENT ON COLUMN authors.created_at IS 'When the author was first stored.';
COMMENT ON COLUMN authors.updated_at IS 'When the author''s counts, dates or name last changed.';

COMMENT ON TABLE author_names IS 'The names each author sent mail under, with how many messages used each.';
COMMENT ON COLUMN author_names.author_id IS 'The author (authors.id).';
COMMENT ON COLUMN author_names.name IS 'A From display name the author used.';
COMMENT ON COLUMN author_names.count IS 'Messages sent under that name.';

COMMENT ON TABLE author_emails IS 'Senders'' addresses and their authors, for matching mail to authors only: pgh_reader can''t read it, and the graph doesn''t use it.';
COMMENT ON COLUMN author_emails.email IS 'A sender''s address, lowercased.';
COMMENT ON COLUMN author_emails.author_id IS 'The author it belongs to (authors.id).';

COMMENT ON TABLE threads IS 'Mailing-list threads: the messages that In-Reply-To and References link, or a reply''s subject without them, named by the root''s Message-ID.';
COMMENT ON COLUMN threads.id IS 'The thread''s ID, which messages.thread_id and the extracts'' thread_id name; the graph keys its vertex thread:<id>.';
COMMENT ON COLUMN threads.list IS 'The mailing list, such as pgsql-hackers.';
COMMENT ON COLUMN threads.root_message_id IS 'The first message''s Message-ID, without angle brackets, even when that message isn''t stored (the first References entry).';
COMMENT ON COLUMN threads.subject IS 'The root''s subject if stored, else the earliest message''s, without reply prefixes and list tags.';
COMMENT ON COLUMN threads.started_at IS 'When its earliest message was sent.';
COMMENT ON COLUMN threads.last_at IS 'When its latest message was sent.';
COMMENT ON COLUMN threads.message_count IS 'Its messages.';
COMMENT ON COLUMN threads.participant_count IS 'The distinct authors of its messages.';
COMMENT ON COLUMN threads.content_version IS 'Bumped when its messages change; extraction records the version it read.';
COMMENT ON COLUMN threads.extraction_state IS 'pending until extracted, then extracted, or skipped when the extractor found nothing to extract; a thread whose content_version passed extracted_version is extracted again.';
COMMENT ON COLUMN threads.extracted_version IS 'The content_version its last extraction read.';
COMMENT ON COLUMN threads.extracted_at IS 'When it was last extracted or skipped.';
COMMENT ON COLUMN threads.created_at IS 'When the thread was first stored.';
COMMENT ON COLUMN threads.updated_at IS 'When its counts, dates or subject last changed.';
COMMENT ON COLUMN threads.subject_key IS 'The subject without reply prefixes, list tags and patch tags, lowercased: what matches a reply without threading headers with its thread.';

COMMENT ON TABLE messages IS 'Mailing-list messages, one per Message-ID, with their text as sent and cleaned for search.';
COMMENT ON COLUMN messages.id IS 'The message''s ID, which the message_id columns of patches, anchors, message_chunks, citations and code_commit_threads name: not its Message-ID.';
COMMENT ON COLUMN messages.message_id IS 'Its Message-ID, without angle brackets: what answers cite; the graph keys its vertex message:<Message-ID>.';
COMMENT ON COLUMN messages.list IS 'The mailing list it came from, such as pgsql-hackers.';
COMMENT ON COLUMN messages.thread_id IS 'Its thread (threads.id).';
COMMENT ON COLUMN messages.parent_message_id IS 'The Message-ID it replies to: In-Reply-To, else the last References entry; IDs that name no message, such as senders'' addresses, are left out.';
COMMENT ON COLUMN messages.references_ids IS 'Its References Message-IDs in order, without angle brackets; IDs that name no message, such as senders'' addresses, are left out.';
COMMENT ON COLUMN messages.author_id IS 'Its sender (authors.id).';
COMMENT ON COLUMN messages.from_name IS 'The From display name, as the public archive shows it.';
COMMENT ON COLUMN messages.subject IS 'Its Subject header, decoded.';
COMMENT ON COLUMN messages.sent_at IS 'When it was sent: its Date header when plausible next to the archive''s date, else the archive''s date.';
COMMENT ON COLUMN messages.body IS 'Its text/plain body, decoded.';
COMMENT ON COLUMN messages.body_clean IS 'The body without quoted replies, signatures and inline patches: what search indexes.';
COMMENT ON COLUMN messages.has_patch IS 'Whether it carries a patch (patches).';
COMMENT ON COLUMN messages.archive_month IS 'The monthly mbox it came from, YYYY-MM; NULL for mail retrieved from thread pages.';
COMMENT ON COLUMN messages.source_id IS 'The source that retrieved it (sources.id).';
COMMENT ON COLUMN messages.raw_sha256 IS 'The SHA-256 of the raw message as retrieved.';
COMMENT ON COLUMN messages.raw_size IS 'The raw message''s size in bytes.';
COMMENT ON COLUMN messages.created_at IS 'When it was stored.';

COMMENT ON TABLE patches IS 'Patches posted to the lists: each attachment or diff in a message''s body that changes files.';
COMMENT ON COLUMN patches.id IS 'The patch''s ID.';
COMMENT ON COLUMN patches.message_id IS 'The message that carries it (messages.id).';
COMMENT ON COLUMN patches.filename IS 'The attachment''s name, or inline-<n> for the n-th diff in the body.';
COMMENT ON COLUMN patches.part IS 'Its number in its set, from the file name: 1 for v3-0001-...; NULL when the name gives none.';
COMMENT ON COLUMN patches.paths IS 'The tree paths it changes.';
COMMENT ON COLUMN patches.added IS 'Lines it adds.';
COMMENT ON COLUMN patches.removed IS 'Lines it removes.';
COMMENT ON COLUMN patches.content IS 'The patch''s text.';
COMMENT ON COLUMN patches.text_ids IS 'Its lines in order, as patch_texts ids; NULL until the patch_lines job indexes it.';
COMMENT ON COLUMN patches.series_version IS 'The version it was posted as, as its file name or subject writes it, without the v: 3, 3.1, 6a, 01, 20261003; NULL when it gives none.';

COMMENT ON TABLE patch_texts IS 'Each distinct line of the patches once, by kind, without its diff prefix: patches.text_ids lists a patch''s lines in order.';
COMMENT ON COLUMN patch_texts.id IS 'The line''s ID, which patches.text_ids lists.';
COMMENT ON COLUMN patch_texts.kind IS '+ added, - removed, a space for context, h for any other line: headers, hunk ranges, text around the diff.';
COMMENT ON COLUMN patch_texts.line IS 'The line without its diff prefix.';

COMMENT ON TABLE anchors IS 'What messages mention that exists outside the archive: files in the tree, commits, CommitFest entries and major releases.';
COMMENT ON COLUMN anchors.id IS 'The anchor''s ID.';
COMMENT ON COLUMN anchors.message_id IS 'The message that mentions it (messages.id).';
COMMENT ON COLUMN anchors.kind IS 'file, commit, commitfest or release.';
COMMENT ON COLUMN anchors.value IS 'As the message gives it: a tree path, a lowercase abbreviated or full commit SHA, a CommitFest entry ID, or a major version such as 19 or 9.6.';
COMMENT ON COLUMN anchors.resolved IS 'Where the commit history has it: a file''s path for the code tools (/branch/<newest release branch>/<path>, else /branch/master/<path>, else /commit/<sha>/<path>), or a commit''s full SHA; else NULL.';
COMMENT ON COLUMN anchors.origin IS 'Where the message mentions it: subject, body or patch.';

COMMENT ON TABLE message_chunks IS 'Messages cut for search: the subject and cleaned body in pieces of about 1,200 to 1,800 characters, for BM25 and embeddings.';
COMMENT ON COLUMN message_chunks.id IS 'The chunk''s ID.';
COMMENT ON COLUMN message_chunks.message_id IS 'Its message (messages.id).';
COMMENT ON COLUMN message_chunks.thread_id IS 'Its message''s thread (threads.id).';
COMMENT ON COLUMN message_chunks.ord IS 'Its place in the message, from 0.';
COMMENT ON COLUMN message_chunks.content IS 'Its text; the subject starts the first chunk.';
COMMENT ON COLUMN message_chunks.embedding IS 'The embedding of content for semantic search (1,536 dimensions); NULL until the embed job embeds it.';

-- Extracts.

COMMENT ON TABLE features IS 'Capabilities or changes to PostgreSQL that threads propose, review, commit, revert or reject, extracted by a model with cited quotes (citations).';
COMMENT ON COLUMN features.id IS 'The feature''s ID, which citations name with extract_kind feature.';
COMMENT ON COLUMN features.slug IS 'Its unique key, such as sql-property-graph-queries; the graph keys its vertex feature:<slug>.';
COMMENT ON COLUMN features.name IS 'Its name.';
COMMENT ON COLUMN features.description IS 'What it does.';
COMMENT ON COLUMN features.status IS 'The most advanced any of its threads showed: proposed, in_progress, committed, reverted, rejected, returned (with feedback, in a CommitFest), withdrawn or unknown.';
COMMENT ON COLUMN features.release IS 'The major version it targets or shipped in, by the name it shipped as (9.0, not its development name 8.5), from its thread with the latest message.';
COMMENT ON COLUMN features.merged_into IS 'The feature it was merged into (features.id), which now has its threads, decisions and positions; NULL unless merged.';
COMMENT ON COLUMN features.embedding IS 'The embedding of search_text for semantic search (1,536 dimensions).';
COMMENT ON COLUMN features.search_text IS 'The name and description, which full-text search ranks.';
COMMENT ON COLUMN features.created_by IS 'The PilotSwarm session that extracted it.';
COMMENT ON COLUMN features.model IS 'The model that extracted it.';
COMMENT ON COLUMN features.created_at IS 'When it was extracted.';
COMMENT ON COLUMN features.updated_at IS 'When it last changed.';
COMMENT ON COLUMN features.release_at IS 'When the evidence for its release was written: the latest message of the thread that gave it.';

COMMENT ON TABLE decisions IS 'Resolutions that threads reached, extracted by a model with cited quotes (citations).';
COMMENT ON COLUMN decisions.id IS 'The decision''s ID, which citations name with extract_kind decision.';
COMMENT ON COLUMN decisions.feature_id IS 'The feature it decides (features.id), if any.';
COMMENT ON COLUMN decisions.thread_id IS 'The thread that reached it (threads.id).';
COMMENT ON COLUMN decisions.title IS 'Its title.';
COMMENT ON COLUMN decisions.outcome IS 'What was decided.';
COMMENT ON COLUMN decisions.alternatives IS 'The options considered but not taken; empty when none.';
COMMENT ON COLUMN decisions.rationale IS 'Why it was decided so.';
COMMENT ON COLUMN decisions.decided_at IS 'When it was decided, as extracted, else the date of the first message it cites.';
COMMENT ON COLUMN decisions.merged_into IS 'The decision it was merged into (decisions.id), which now has its positions; NULL unless merged.';
COMMENT ON COLUMN decisions.embedding IS 'The embedding of search_text for semantic search (1,536 dimensions).';
COMMENT ON COLUMN decisions.search_text IS 'The title, outcome, rationale and alternatives, which full-text search ranks.';
COMMENT ON COLUMN decisions.created_by IS 'The PilotSwarm session that extracted it.';
COMMENT ON COLUMN decisions.model IS 'The model that extracted it.';
COMMENT ON COLUMN decisions.created_at IS 'When it was extracted.';
COMMENT ON COLUMN decisions.updated_at IS 'When it last changed.';

COMMENT ON TABLE positions IS 'Authors'' stances in threads on features or decisions, extracted by a model with cited quotes (citations).';
COMMENT ON COLUMN positions.id IS 'The position''s ID, which citations name with extract_kind position.';
COMMENT ON COLUMN positions.author_id IS 'The author who took it (authors.id).';
COMMENT ON COLUMN positions.thread_id IS 'The thread where they took it (threads.id).';
COMMENT ON COLUMN positions.feature_id IS 'The feature it is about (features.id); it is about a feature, a decision or both.';
COMMENT ON COLUMN positions.decision_id IS 'The decision it is about (decisions.id); it is about a feature, a decision or both.';
COMMENT ON COLUMN positions.stance IS 'proposes, supports, opposes, concerned, neutral or reviews.';
COMMENT ON COLUMN positions.summary IS 'What the author said, in brief.';
COMMENT ON COLUMN positions.embedding IS 'The embedding of search_text for semantic search (1,536 dimensions).';
COMMENT ON COLUMN positions.search_text IS 'The summary, which full-text search ranks.';
COMMENT ON COLUMN positions.created_by IS 'The PilotSwarm session that extracted it.';
COMMENT ON COLUMN positions.model IS 'The model that extracted it.';
COMMENT ON COLUMN positions.created_at IS 'When it was extracted.';

COMMENT ON TABLE citations IS 'The quotes from messages that support each feature, decision and position.';
COMMENT ON COLUMN citations.id IS 'The citation''s ID.';
COMMENT ON COLUMN citations.extract_kind IS 'feature, decision or position: the table whose ID extract_id is.';
COMMENT ON COLUMN citations.extract_id IS 'The feature''s, decision''s or position''s ID.';
COMMENT ON COLUMN citations.message_id IS 'The message quoted (messages.id).';
COMMENT ON COLUMN citations.quote IS 'Text quoted verbatim from the message.';

COMMENT ON TABLE feature_threads IS 'The threads that discuss each feature: the graph''s DISCUSSES edges.';
COMMENT ON COLUMN feature_threads.feature_id IS 'The feature (features.id).';
COMMENT ON COLUMN feature_threads.thread_id IS 'A thread that discusses it (threads.id).';

COMMENT ON TABLE feature_files IS 'The files that implement each feature, as its threads'' patches showed: the graph''s IMPLEMENTS edges.';
COMMENT ON COLUMN feature_files.feature_id IS 'The feature (features.id).';
COMMENT ON COLUMN feature_files.path IS 'A file''s path in the tree.';
COMMENT ON COLUMN feature_files.thread_id IS 'The thread whose patches showed it (threads.id), if known.';

COMMENT ON TABLE extraction_leases IS 'Threads leased to extractors by next_extraction_batch: no other extractor gets a thread while its lease is active.';
COMMENT ON COLUMN extraction_leases.token IS 'The lease''s token, which the extractor sends with each call.';
COMMENT ON COLUMN extraction_leases.thread_ids IS 'The threads leased (threads.id).';
COMMENT ON COLUMN extraction_leases.session IS 'The extractor''s PilotSwarm session.';
COMMENT ON COLUMN extraction_leases.model IS 'The extractor''s model.';
COMMENT ON COLUMN extraction_leases.leased_at IS 'When the threads were leased.';
COMMENT ON COLUMN extraction_leases.expires_at IS 'When the lease lapses; each call that uses it renews it.';
COMMENT ON COLUMN extraction_leases.released_at IS 'When each of its threads had been submitted or skipped; NULL until then.';
COMMENT ON COLUMN extraction_leases.thread_versions IS 'Each leased thread''s content_version when leased, in thread_ids'' order: what submit_extracts records as extracted; NULL for leases from before this column.';

COMMENT ON TABLE drafts IS 'Draft features and decisions with the existing extracts they resemble: submit_extracts takes only drafts that match_extracts saw.';
COMMENT ON COLUMN drafts.id IS 'The draft''s ID.';
COMMENT ON COLUMN drafts.lease_token IS 'The lease whose extractor wrote it (extraction_leases.token).';
COMMENT ON COLUMN drafts.thread_id IS 'The thread it comes from (threads.id).';
COMMENT ON COLUMN drafts.kind IS 'feature or decision.';
COMMENT ON COLUMN drafts.draft IS 'The draft as the extractor wrote it.';
COMMENT ON COLUMN drafts.embedding IS 'The draft''s embedding, which finds the extracts it resembles.';
COMMENT ON COLUMN drafts.candidates IS 'The existing extracts it resembles, as match_extracts returned them.';
COMMENT ON COLUMN drafts.created_at IS 'When match_extracts saw it.';

COMMENT ON TABLE distinct_pairs IS 'Pairs of features, or of decisions, judged to be different things, which find_duplicates doesn''t offer again.';
COMMENT ON COLUMN distinct_pairs.kind IS 'feature or decision.';
COMMENT ON COLUMN distinct_pairs.low_id IS 'The pair''s lower ID.';
COMMENT ON COLUMN distinct_pairs.high_id IS 'The pair''s higher ID.';
COMMENT ON COLUMN distinct_pairs.reason IS 'Why they differ.';
COMMENT ON COLUMN distinct_pairs.judged_by IS 'extractor, when an extractor created one despite the other (its distinct_because), or dedup (mark_distinct).';
COMMENT ON COLUMN distinct_pairs.session IS 'The PilotSwarm session that judged them.';
COMMENT ON COLUMN distinct_pairs.model IS 'The model that judged them.';
COMMENT ON COLUMN distinct_pairs.created_at IS 'When they were judged.';

COMMENT ON TABLE duplicate_candidates IS 'Pairs of features, or of decisions, that look alike, found as each extract arrives: what find_duplicates lists.';
COMMENT ON COLUMN duplicate_candidates.kind IS 'feature or decision.';
COMMENT ON COLUMN duplicate_candidates.low_id IS 'The pair''s lower ID.';
COMMENT ON COLUMN duplicate_candidates.high_id IS 'The pair''s higher ID.';
COMMENT ON COLUMN duplicate_candidates.semantic IS 'The cosine similarity of their embeddings; NULL while either has none.';
COMMENT ON COLUMN duplicate_candidates.name_similarity IS 'The trigram similarity of their names (a decision''s title).';
COMMENT ON COLUMN duplicate_candidates.found_at IS 'When the pair was found.';

COMMENT ON TABLE duplicate_checks IS 'The features and decisions already compared with the others for duplicates.';
COMMENT ON COLUMN duplicate_checks.kind IS 'feature or decision.';
COMMENT ON COLUMN duplicate_checks.extract_id IS 'The feature''s or decision''s ID.';
COMMENT ON COLUMN duplicate_checks.semantic IS 'Whether by embedding too: one compared before it had an embedding is compared again.';
COMMENT ON COLUMN duplicate_checks.checked_at IS 'When it was compared.';

COMMENT ON TABLE merge_log IS 'Merges of authors, features and decisions, with what each re-pointed, so that undo_merge can reverse them.';
COMMENT ON COLUMN merge_log.id IS 'The merge''s ID, which undo_merge takes.';
COMMENT ON COLUMN merge_log.kind IS 'author, feature or decision.';
COMMENT ON COLUMN merge_log.from_id IS 'The one merged away, whose merged_into is into_id.';
COMMENT ON COLUMN merge_log.into_id IS 'The one it was merged into.';
COMMENT ON COLUMN merge_log.moved IS 'What the merge re-pointed, which undo_merge restores.';
COMMENT ON COLUMN merge_log.reason IS 'Why they are the same.';
COMMENT ON COLUMN merge_log.actor IS 'Who merged them, such as crawler.';
COMMENT ON COLUMN merge_log.created_at IS 'When they were merged.';
COMMENT ON COLUMN merge_log.reverted_at IS 'When undo_merge reversed the merge; NULL while it stands.';

-- The commit history.

COMMENT ON TABLE code_history IS 'The commit history''s build and syncs, in one row: the git repository it comes from, master''s head and the last sync.';
COMMENT ON COLUMN code_history.id IS 'Always 1.';
COMMENT ON COLUMN code_history.master IS 'The branch the others fork from: master.';
COMMENT ON COLUMN code_history.url IS 'The repository that syncs fetch new commits from; NULL for a local clone only.';
COMMENT ON COLUMN code_history.paths IS 'The path prefixes kept; empty keeps the whole tree.';
COMMENT ON COLUMN code_history.built_at IS 'When the history was built.';
COMMENT ON COLUMN code_history.synced_at IS 'When it last synced, which applies new commits; NULL before the first sync.';
COMMENT ON COLUMN code_history.head IS 'Master''s newest commit''s SHA.';
COMMENT ON COLUMN code_history.weave_epoch IS 'Bumped when weave keys are renumbered, which invalidates cached line numbers.';
COMMENT ON COLUMN code_history.stats IS 'What the build did: its walk''s and store''s counts, and how long each step took in seconds.';

COMMENT ON TABLE code_lanes IS 'Master and each branch, with the block of commit addresses it owns: the n-th branch forked at master commit f owns [f * 2^32 + n * 2^24 + 1, f * 2^32 + (n + 1) * 2^24].';
COMMENT ON COLUMN code_lanes.id IS 'The lane''s ID (code_commits.lane).';
COMMENT ON COLUMN code_lanes.name IS 'The branch: master, REL_17_STABLE and so on.';
COMMENT ON COLUMN code_lanes.fork_addr IS 'The address of the master commit it forks at; NULL for master.';
COMMENT ON COLUMN code_lanes.lo IS 'The first address of its block.';
COMMENT ON COLUMN code_lanes.hi IS 'The end of its block, exclusive.';
COMMENT ON COLUMN code_lanes.head_addr IS 'The address of its newest commit, or of its fork point while it has none of its own.';

COMMENT ON TABLE code_commits IS 'Every commit of every branch and tag of PostgreSQL''s git repository.';
COMMENT ON COLUMN code_commits.addr IS 'The commit''s address: master''s k-th commit is k * 2^32, and a branch''s commits follow the master commit it forks at; code_commit_files, code_refs and the lo and hi of versions and lines name commits by address.';
COMMENT ON COLUMN code_commits.sha IS 'The full commit SHA; the graph keys its vertex commit:<sha>.';
COMMENT ON COLUMN code_commits.lane IS 'Its branch (code_lanes.id).';
COMMENT ON COLUMN code_commits.committed_at IS 'The commit date.';
COMMENT ON COLUMN code_commits.authored_at IS 'The author date.';
COMMENT ON COLUMN code_commits.author_name IS 'The author''s name, without an address: in PostgreSQL''s history, the committer''s; Author trailers credit patch authors.';
COMMENT ON COLUMN code_commits.committer_name IS 'The committer''s name, without an address.';
COMMENT ON COLUMN code_commits.subject IS 'The message''s first line.';
COMMENT ON COLUMN code_commits.message IS 'The whole message, subject first.';
COMMENT ON COLUMN code_commits.trailers IS 'Its trailers by key, values in order: Discussion as written (postgr.es/m URLs or Message-IDs), people by name, the others whole; keys such as Discussion, Author, Reviewed-by, Reported-by and Backpatch-through.';
COMMENT ON COLUMN code_commits.patch_id IS 'A hash of its changes without whitespace, line numbers or context, whatever the order of files; NULL when no file changed.';
COMMENT ON COLUMN code_commits.backpatch IS 'The address of the first commit of its back-patch group (master''s, when the group has one), which commits applying one change to several branches share; NULL for a commit alone.';

COMMENT ON TABLE code_commit_files IS 'What each commit changed, file by file, as git show --stat counts it.';
COMMENT ON COLUMN code_commit_files.addr IS 'The commit (code_commits.addr).';
COMMENT ON COLUMN code_commit_files.path IS 'The file''s path.';
COMMENT ON COLUMN code_commit_files.old_path IS 'Where a rename moved the file from; NULL otherwise.';
COMMENT ON COLUMN code_commit_files.status IS 'A added, D deleted, M modified or R renamed.';
COMMENT ON COLUMN code_commit_files.added IS 'Lines added.';
COMMENT ON COLUMN code_commit_files.removed IS 'Lines removed.';
COMMENT ON COLUMN code_commit_files.is_binary IS 'Whether either side is binary, with no lines counted.';
COMMENT ON COLUMN code_commit_files.version IS 'The version the commit made (code_file_versions.id); NULL when it deleted the file.';
COMMENT ON COLUMN code_commit_files.old_version IS 'The version the commit ended (code_file_versions.id); NULL when it added the file.';

COMMENT ON TABLE code_commit_threads IS 'Commits and the pgsql-hackers threads that discussed them, found again after each build and sync.';
COMMENT ON COLUMN code_commit_threads.sha IS 'The commit (code_commits.sha).';
COMMENT ON COLUMN code_commit_threads.thread_id IS 'A thread that discussed it (threads.id).';
COMMENT ON COLUMN code_commit_threads.via IS 'trailer, when its Discussion trailer names a message of the thread, or anchor, when a message of the thread cites it by SHA.';
COMMENT ON COLUMN code_commit_threads.message_id IS 'The message named, or the message citing the commit (messages.id).';

COMMENT ON TABLE code_refs IS 'The repository''s branches and tags, and the commits they name.';
COMMENT ON COLUMN code_refs.kind IS 'branch or tag.';
COMMENT ON COLUMN code_refs.name IS 'Its name, such as REL_17_STABLE or REL_17_0.';
COMMENT ON COLUMN code_refs.addr IS 'The commit it names (code_commits.addr).';

COMMENT ON TABLE code_lineages IS 'Files'' identities across whole-file renames (git''s 50% similarity).';
COMMENT ON COLUMN code_lineages.id IS 'The lineage''s ID, which code_file_versions.lineage and code_line_instances.lineage name.';
COMMENT ON COLUMN code_lineages.first_path IS 'The file''s first path.';
COMMENT ON COLUMN code_lineages.born IS 'The address of the commit that added the file.';

COMMENT ON TABLE code_file_versions IS 'Files'' contents at their paths over time: commit X''s tree is the versions with lo <= X AND X < hi AND (alive IS NULL OR alive @> X).';
COMMENT ON COLUMN code_file_versions.id IS 'The version''s ID.';
COMMENT ON COLUMN code_file_versions.lineage IS 'The file''s lineage (code_lineages.id).';
COMMENT ON COLUMN code_file_versions.path IS 'The file''s path.';
COMMENT ON COLUMN code_file_versions.blob_sha IS 'The git blob SHA of its content.';
COMMENT ON COLUMN code_file_versions.lo IS 'The address of the commit that made it.';
COMMENT ON COLUMN code_file_versions.hi IS 'The address from which it is gone, exclusive; 9223372036854775807 when it is alive at every later master commit.';
COMMENT ON COLUMN code_file_versions.alive IS 'The addresses within [lo, hi) where it is alive, set only when a branch deleted it; NULL means all of them.';
COMMENT ON COLUMN code_file_versions.size IS 'Its size in bytes.';
COMMENT ON COLUMN code_file_versions.line_count IS 'Its lines.';
COMMENT ON COLUMN code_file_versions.is_binary IS 'Binary to the code tools: a NUL byte or invalid UTF-8.';
COMMENT ON COLUMN code_file_versions.final_newline IS 'Whether it ends with a newline.';
COMMENT ON COLUMN code_file_versions.bom IS 'Whether it starts with a UTF-8 byte order mark.';
COMMENT ON COLUMN code_file_versions.subsystem IS 'The subsystem its path belongs to: parser, utils/adt, nodes, contrib/pg_trgm and so on; NULL for a file in none, such as one at the top.';

COMMENT ON TABLE code_line_instances IS 'Every line of every file version, stored once: a changed line is a new instance; a version''s lines are its lineage''s instances alive at its lo, in weave order.';
COMMENT ON COLUMN code_line_instances.weave IS 'The line''s place in its lineage, consistent with every version of the file.';
COMMENT ON COLUMN code_line_instances.lo IS 'The address of the commit that added the line.';
COMMENT ON COLUMN code_line_instances.hi IS 'The address from which it is gone, exclusive; 9223372036854775807 when it is alive at every later master commit.';
COMMENT ON COLUMN code_line_instances.id IS 'The instance''s ID.';
COMMENT ON COLUMN code_line_instances.lineage IS 'The file''s lineage (code_lineages.id).';
COMMENT ON COLUMN code_line_instances.bytes IS 'The line''s length in the file in bytes, for byte offsets.';
COMMENT ON COLUMN code_line_instances.alive IS 'The addresses within [lo, hi) where it is alive, set only when a branch deleted it; NULL means all of them.';
COMMENT ON COLUMN code_line_instances.replaces IS 'The instance it replaced one-for-one, which blame follows through ignored revisions; NULL for none.';
COMMENT ON COLUMN code_line_instances.line IS 'The line without its terminator; a CR before LF stays.';

-- The graph's bookkeeping.

COMMENT ON TABLE graph_label_counts IS 'Each vertex label''s and edge type''s count in pghacker_graph, which the graph sync keeps.';
COMMENT ON COLUMN graph_label_counts.label IS 'The vertex label or edge type, such as Message or AUTHORED.';
COMMENT ON COLUMN graph_label_counts.kind IS 'vertex or edge.';
COMMENT ON COLUMN graph_label_counts.count IS 'How many the graph has.';

COMMENT ON TABLE graph_nodes IS 'The vertex each key became in pghacker_graph, so that a sync changes only what changed.';
COMMENT ON COLUMN graph_nodes.key IS 'The vertex''s key, such as message:<Message-ID>, author:<id> or file:<path>.';
COMMENT ON COLUMN graph_nodes.label IS 'Its label, such as Message.';
COMMENT ON COLUMN graph_nodes.graph_id IS 'Its AGE vertex ID.';
COMMENT ON COLUMN graph_nodes.props IS 'Its properties.';
COMMENT ON COLUMN graph_nodes.updated_at IS 'When the sync last wrote it.';

COMMENT ON TABLE graph_edges IS 'The edge each label and pair of keys became in pghacker_graph, so that a sync changes only what changed.';
COMMENT ON COLUMN graph_edges.label IS 'Its type, such as AUTHORED.';
COMMENT ON COLUMN graph_edges.start_key IS 'Its start vertex''s key (graph_nodes.key).';
COMMENT ON COLUMN graph_edges.end_key IS 'Its end vertex''s key (graph_nodes.key).';
COMMENT ON COLUMN graph_edges.graph_id IS 'Its AGE edge ID.';
COMMENT ON COLUMN graph_edges.props IS 'Its properties.';

COMMENT ON TABLE graph_changes IS 'The vertices and edges each write may affect, which the incremental graph sync recomputes and then deletes.';
COMMENT ON COLUMN graph_changes.id IS 'The change''s place in the log.';
COMMENT ON COLUMN graph_changes.label IS 'The edge''s type, or NULL for a vertex.';
COMMENT ON COLUMN graph_changes.key IS 'The vertex''s key, or the edge''s start key.';
COMMENT ON COLUMN graph_changes.end_key IS 'The edge''s end key, or NULL for a vertex.';
COMMENT ON COLUMN graph_changes.at IS 'When it was recorded.';

COMMENT ON TABLE graph_sync_state IS 'The requests for a full graph sync, and the last one, in one row.';
COMMENT ON COLUMN graph_sync_state.id IS 'Always true.';
COMMENT ON COLUMN graph_sync_state.full_requested IS 'Bumped by each request for a full sync.';
COMMENT ON COLUMN graph_sync_state.full_done IS 'The requests that the last full sync saw before it read the tables: one is due while this is below full_requested.';
COMMENT ON COLUMN graph_sync_state.full_reason IS 'Why the latest full sync was requested.';
COMMENT ON COLUMN graph_sync_state.full_requested_at IS 'When the latest full sync was requested.';
COMMENT ON COLUMN graph_sync_state.last_full_at IS 'When the last full sync ended.';
COMMENT ON COLUMN graph_sync_state.projection IS 'The projection the last full sync computed; a newer one needs a full sync.';

COMMENT ON TABLE graph_sync_runs IS 'The last 100 graph syncs and what each changed.';
COMMENT ON COLUMN graph_sync_runs.id IS 'The run''s ID.';
COMMENT ON COLUMN graph_sync_runs.started_at IS 'When it was recorded, as finished_at.';
COMMENT ON COLUMN graph_sync_runs.finished_at IS 'When it was recorded.';
COMMENT ON COLUMN graph_sync_runs.stats IS 'What it changed.';

COMMENT ON TABLE graph_committers IS 'The author whose COMMITTED edges reach each commit author name''s commits, which the graph sync keeps to find the names whose author changed.';
COMMENT ON COLUMN graph_committers.name IS 'A commit author name, lowercased.';
COMMENT ON COLUMN graph_committers.author_key IS 'The author''s graph key, author:<id>.';

-- The indexer.

COMMENT ON TABLE jobs IS 'The indexer''s job queue: one job per kind and key, which its workers claim and run.';
COMMENT ON COLUMN jobs.id IS 'The job''s ID.';
COMMENT ON COLUMN jobs.source_id IS 'The source it retrieves for (sources.id); NULL for other jobs.';
COMMENT ON COLUMN jobs.kind IS 'What it does: mbox_month, seed_thread, embed, graph_sync, code_history_sync, patch_lines, duplicates and so on.';
COMMENT ON COLUMN jobs.key IS 'Its identity within its kind, such as pgsql-hackers/2026-06 for an mbox_month, or recurring for a recurring job.';
COMMENT ON COLUMN jobs.payload IS 'Its arguments.';
COMMENT ON COLUMN jobs.state IS 'queued, running, done, failed or cancelled.';
COMMENT ON COLUMN jobs.priority IS 'Queued jobs of higher priority run first.';
COMMENT ON COLUMN jobs.attempts IS 'Times a worker has claimed it.';
COMMENT ON COLUMN jobs.max_attempts IS 'The attempts after which it fails for good.';
COMMENT ON COLUMN jobs.run_after IS 'When it may run next.';
COMMENT ON COLUMN jobs.locked_by IS 'The worker that claimed it.';
COMMENT ON COLUMN jobs.locked_at IS 'When that worker claimed it.';
COMMENT ON COLUMN jobs.last_error IS 'Its last failure''s error.';
COMMENT ON COLUMN jobs.result IS 'What its last run reported.';
COMMENT ON COLUMN jobs.created_at IS 'When it was first queued.';
COMMENT ON COLUMN jobs.finished_at IS 'When it last finished, failed or was cancelled.';

COMMENT ON TABLE schema_migrations IS 'The migrations applied, with their checksums.';
COMMENT ON COLUMN schema_migrations.version IS 'The migration''s file name without .sql, such as 0023_schema_comments.';
COMMENT ON COLUMN schema_migrations.sha256 IS 'The SHA-256 of its SQL without the license header: a migration changed after it was applied is refused.';
COMMENT ON COLUMN schema_migrations.applied_at IS 'When it was applied.';
