# Wave 1 evaluation

20 of 20 questions passed (judged correct, with valid verbatim citations).
Answer model: `github-copilot:gpt-6-luna`; judge: `github-copilot:gpt-6-luna`; run 2026-10-09T10:54:49.510Z.

| Question | Kind | Correct | Citations valid | On a gold thread | Seconds |
| --- | --- | --- | --- | --- | --- |
| q01: Why were the SQL/JSON features reverted from PostgreSQL 15, and who carried out the revert? | decision | yes | yes | yes | 42 |
| q02: What caused the PostgreSQL 15 performance regression that Mark Callaghan reported in June 2022, and how was it fixed? | history | yes | yes | yes | 99 |
| q03: When was JSON_TABLE committed to PostgreSQL, by whom, and which part of the original proposal was left out? | history | yes | yes | yes | 46 |
| q04: What is Andres Freund's perspective on the SQL/PGQ (property graph queries) implementation? | position | yes | yes | yes | 32 |
| q05: Is SQL/PGQ (property graphs and GRAPH_TABLE) part of PostgreSQL 19? What happened to it? | decision | yes | yes | yes | 55 |
| q06: Does PostgreSQL 19 support the JSON_TABLE PLAN clause? Check both the source code and the mailing list. | code-history | yes | yes | yes | 41 |
| q07: In PostgreSQL 19's pg_trgm, what are the default thresholds for similarity, word similarity and strict word similarity, which operators use them, and which settings change them? | code | yes | yes | no | 39 |
| q08: What limit did the row pattern recognition (RPR) patch set on the number of pattern variables in June 2026, and why that number? | decision | yes | yes | yes | 21 |
| q09: In the proposal for a logical replication conflict log table, which explicit lock modes may users take on conflict log tables, and why? | decision | yes | yes | yes | 27 |
| q10: Cirrus CI shut down on June 1, 2026. What replaced it for PostgreSQL's CI, and how did the hackers handle CI runs in the many forks of the PostgreSQL repository? | decision | yes | yes | yes | 40 |
| q11: How does a caller choose which kinds of DDL the proposed pg_get_table_ddl() function emits? | decision | yes | yes | yes | 49 |
| q12: What race between the sequencesync worker and ALTER SUBSCRIPTION ... REFRESH SEQUENCES was found in July 2026, and how was it fixed? | decision | yes | yes | yes | 61 |
| q13: According to the June 2026 discussion of 'The PostgreSQL C Dialect' wiki page, which C standard does PostgreSQL 19 require, and should new code use Size or size_t? | decision | yes | yes | yes | 21 |
| q14: What did pgsql-hackers decide about the refint contrib module in June 2026, and why? | decision | yes | yes | yes | 32 |
| q15: What was Robert Haas's view of the SQL/JSON features in PostgreSQL 15 during the August 2022 debate? | position | yes | yes | yes | 25 |
| q16: What is Tom Lane's position on requiring a newer Python than the operating system distributions package, for PostgreSQL's planned pytest-based tests? | position | yes | yes | yes | 19 |
| q17: In the PostgreSQL 19 source, which parser function transforms a raw JSON_TABLE clause, into what node, and where is it defined? | code | yes | yes | no | 18 |
| q18: In PostgreSQL 19, how is a JSON_TABLE column represented in the raw parse tree, and what column types can it have? | code | yes | yes | no | 24 |
| q19: Who were the three most active posters on pgsql-hackers in June and July 2026, by number of messages, and how many messages did each send? | sql | yes | yes | no | 15 |
| q20: Which three source files of PostgreSQL's parser (src/backend/parser) are mentioned in the most pgsql-hackers messages in the corpus? | graph | yes | yes | no | 38 |
