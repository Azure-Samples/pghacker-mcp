# pghacker-mcp evaluation

24 of 30 questions passed (judged correct, with valid verbatim citations and, for questions with gold commits, a valid cited gold commit).
Questions with gold commits: 7 of 10 passed; the others: 17 of 20.
Answer model: `github-copilot:gpt-6-luna`; judge: `github-copilot:gpt-6-luna`; run 2026-10-14T20:24:27.179Z.

pghacker-mcp: the deployment's public `/mcp` endpoint (environment-specific URL omitted).

| Question | Kind | Correct | Citations valid | On a gold thread | Commits valid | Gold commit cited | Seconds |
| --- | --- | --- | --- | --- | --- | --- | --- |
| q01: Why were the SQL/JSON features reverted from PostgreSQL 15, and who carried out the revert? | decision | no | yes | no | yes | — | 84 |
| q02: What caused the PostgreSQL 15 performance regression that Mark Callaghan reported in June 2022, and how was it fixed? | history | yes | yes | yes | yes | — | 59 |
| q03: When was JSON_TABLE committed to PostgreSQL, by whom, and which part of the original proposal was left out? | history | yes | yes | yes | yes | — | 51 |
| q04: What is Andres Freund's perspective on the SQL/PGQ (property graph queries) implementation? | position | yes | yes | yes | — | — | 75 |
| q05: Is SQL/PGQ (property graphs and GRAPH_TABLE) part of PostgreSQL 19? What happened to it? | decision | yes | yes | yes | yes | — | 49 |
| q06: Does PostgreSQL 19 support the JSON_TABLE PLAN clause? Check both the source code and the mailing list. | code-history | yes | yes | yes | yes | — | 89 |
| q07: In PostgreSQL 19's pg_trgm, what are the default thresholds for similarity, word similarity and strict word similarity, which operators use them, and which settings change them? | code | yes | yes | no | — | — | 29 |
| q08: What limit did the row pattern recognition (RPR) patch set on the number of pattern variables in June 2026, and why that number? | decision | yes | yes | yes | — | — | 43 |
| q09: In the proposal for a logical replication conflict log table, which explicit lock modes may users take on conflict log tables, and why? | decision | yes | yes | yes | — | — | 68 |
| q10: Cirrus CI shut down on June 1, 2026. What replaced it for PostgreSQL's CI, and how did the hackers handle CI runs in the many forks of the PostgreSQL repository? | decision | yes | yes | yes | yes | — | 115 |
| q11: How does a caller choose which kinds of DDL the proposed pg_get_table_ddl() function emits? | decision | yes | yes | yes | — | — | 72 |
| q12: What race between the sequencesync worker and ALTER SUBSCRIPTION ... REFRESH SEQUENCES was found in July 2026, and how was it fixed? | decision | no | yes | yes | yes | — | 59 |
| q13: According to the June 2026 discussion of 'The PostgreSQL C Dialect' wiki page, which C standard does PostgreSQL 19 require, and should new code use Size or size_t? | decision | yes | yes | yes | — | — | 23 |
| q14: What did pgsql-hackers decide about the refint contrib module in June 2026, and why? | decision | yes | yes | yes | yes | — | 70 |
| q15: What was Robert Haas's view of the SQL/JSON features in PostgreSQL 15 during the August 2022 debate? | position | yes | yes | yes | — | — | 56 |
| q16: What is Tom Lane's position on requiring a newer Python than the operating system distributions package, for PostgreSQL's planned pytest-based tests? | position | yes | yes | yes | — | — | 38 |
| q17: In the PostgreSQL 19 source, which parser function transforms a raw JSON_TABLE clause, into what node, and where is it defined? | code | yes | yes | no | — | — | 19 |
| q18: In PostgreSQL 19, how is a JSON_TABLE column represented in the raw parse tree, and what column types can it have? | code | yes | yes | no | — | — | 30 |
| q19: Who were the three most active posters on pgsql-hackers in June and July 2026, by number of messages, and how many messages did each send? | sql | yes | yes | no | — | — | 30 |
| q20: Which three source files of PostgreSQL's parser (src/backend/parser) are mentioned in the most pgsql-hackers messages in the corpus? | graph | no | yes | no | — | — | 34 |
| q21: Which commit added the vacuum_buffer_usage_limit setting? Who wrote it, who reviewed it, and which pgsql-hackers thread discussed it? | origin | no | yes | yes | yes | yes | 98 |
| q22: Who originally wrote the built-in trigger function suppress_redundant_updates_trigger(), when was it committed, and what problem did it address? Note that git blame credits many of its lines to later reformatting. | origin | yes | yes | yes | yes | yes | 67 |
| q23: In June 2025, PostgreSQL fixed a bug where VACUUM could apply the wrong TRUNCATE or INDEX_CLEANUP setting when it processed several relations, or a table and its TOAST table, in one command. Which branches got the fix, and in which release did it first ship on each? | backpatch | no | yes | yes | yes | yes | 37 |
| q24: Was JIT compilation enabled by default in PostgreSQL 11? How did that differ in PostgreSQL 12, and why? | time-travel | yes | yes | yes | yes | yes | 89 |
| q25: What was the default of password_encryption in PostgreSQL 13, what is it in PostgreSQL 14, and why did it change? | time-travel | yes | yes | yes | yes | yes | 40 |
| q26: When and why was recovery.conf removed from PostgreSQL, and what replaced it? | change | yes | yes | yes | yes | yes | 53 |
| q27: Why was the pg_xlog directory renamed, to what, and when? | change | yes | yes | yes | yes | yes | 36 |
| q28: Which posted version of the JSON_TABLE patch did Amit Langote commit as the basic JSON_TABLE() functionality in April 2024, and how did it differ from the full JSON_TABLE patch posted in March 2024? | provenance | yes | yes | yes | yes | yes | 75 |
| q29: Who reported the problem that led PostgreSQL to reject "=" in the names of reloptions and foreign-data options in June 2025, who wrote and reviewed the fix, and why was "=" a problem? | review | yes | yes | yes | yes | yes | 54 |
| q30: Who reported the crash in ALTER SUBSCRIPTION ... REFRESH PUBLICATION when a subscribed relation is dropped concurrently, fixed in August 2026? Who wrote and reviewed the fix, and what caused the crash? | review | no | yes | no | yes | no | 58 |

q21 to q30 need the commit history, which production has read since 2026-10-14. Every question with gold commits cited valid commits, and all but q30 cited a gold one.

The failures, by cause:
- Question problems in the new questions, fixed after this run; rerun alone, all three passed:
  - q21: the gold gave the commit's date in New Zealand, 7 April 2023, and the answer the UTC date, 6 April.
  - q23: the answer said master's fix first shipped in 18beta2, which is right; the gold said only 18.0.
  - q30: two fixes for relations dropped during a subscription refresh landed on 26 August 2026, and the answer described the other (05f37b2b5eac). The question now says the crash was reported in May 2026.
- Question problems in the original questions, fixed afterwards; rerun alone, both passed with judge scores of 1.0:
  - q20: the gold counts (about 240, 160 and 125 messages) predated the corpus's growth to the whole archive. The answer's gram.y (6,243), parse_utilcmd.c (2,032) and parse_expr.c (1,789) match its anchors and graph, which the gold now gives.
  - q12: the gold said the fix relies on the subscription's AccessExclusiveLock. The commit (45cf7b1e5bf9) also stops the running sequencesync worker before resetting the sequences, as the answer said, and the judge counted that against it. The gold now gives both.
- q01 failed here and again alone (judge scores 0.55 and 0.65). Both answers named the RMT and Andrew Dunstan's revert, citing the RMT's thread, but not the objection to subtransactions in expression evaluation, which the corpus holds. It passed on 2026-10-09: answer quality, not a tool fault.
