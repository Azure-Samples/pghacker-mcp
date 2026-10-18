# pghacker-mcp evaluation: reviews as their authors would give them

7 of 10 reviews passed (judged to raise the reviewer's main point, with valid verbatim citations, at least 2 in the reviewer's own words, and none sent on or after the patch's date or from the actual review).
Mean score, the share of the reviewer's actual points raised: 0.50.
Skill: none in the prompt; 10 of 10 reviews loaded it on their own, with `load_skill_review_as_author`.
Answer model: `github-copilot:gpt-6-luna`; judge: `github-copilot:gpt-6-luna`; run 2026-10-18T22:38:26.611Z.

pghacker-mcp: the endpoint given as `--mcp-url`.

| Review | Reviewer | Correct | Score | Citations valid | Reviewer's own | No leak | Loaded the skill | Seconds |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| r01: [PATCH] Add ALTER SYSTEM RELOAD | Tom Lane | yes | 0.50 | yes | 3 | yes | yes | 86 |
| r02: [PATCH] intXshr, intXshl: return error on shift count out of range | Tom Lane | yes | 1.00 | yes | 2 | yes | yes | 52 |
| r03: find_all_inheritors() with NoLock fails on concurrently dropped sub-partitions | Tom Lane | yes | 1.00 | yes | 3 | yes | yes | 61 |
| r04: [PATCH] bufmgr: tighten LWLock:BufferMapping on InvalidateBuffer | Andres Freund | yes | 0.50 | yes | 3 | yes | yes | 104 |
| r05: Show effective xmin in pg_replication_slots when xmin is not set | Andres Freund | no | 0.00 | yes | 4 | yes | yes | 103 |
| r06: Costing for parallel scans with few/single row produced in the outer side | Robert Haas | no | 0.00 | yes | 4 | yes | yes | 107 |
| r07: pg_resetwal: refuse to run when backup_label exists | Michael Paquier | yes | 0.50 | yes | 3 | yes | yes | 95 |
| r08: Re: pg_upgrade --copy-file-range fails with EINVAL on Linux 4.19 | Peter Eisentraut | yes | 1.00 | yes | 2 | yes | yes | 107 |
| r09: Opportunistic pruning is lost under direct io, and nothing shows it | Melanie Plageman | yes | 0.50 | yes | 3 | yes | yes | 129 |
| r10: [PATCH] Add ALTER SYSTEM RELOAD | Chao Li | no | 0.00 | yes | 3 | yes | yes | 141 |

Notes:

- The first run since production serves `load_skill_review_as_author`. The prompt carried no skill, and every review loaded it with the tool before calling any other pghacker-mcp tool. The agents, on PilotSwarm, don't declare MCP's skills extension.
- In r04, r06, r07 and r09, the agent first passed the tool's name to PilotSwarm's own skill tools (`load_skill`, `skill`), which have no such skill, then called the tool.
- The same day, against the same production, the same reviews with the skill in the prompt passed 7 of 10, with a mean score of 0.65; their agents called the tool too, as the instructions say. [RESULTS-reviews.md](RESULTS-reviews.md)'s 0.75 came before production served `author_perspective`'s `paths` and `until`. Without the skill, before the tool, the same agent passed 5 and 6 of 10 ([RESULTS-reviews-baseline.md](RESULTS-reviews-baseline.md)).
- Ten reviews are a small sample: between this run and that one, four reviews passed in one and failed in the other, two each way.
- Every review that failed, in either run, missed the reviewer's objection to the patch's premise or approach, and most expected the reviewer to welcome the patch. r05 failed in both.
