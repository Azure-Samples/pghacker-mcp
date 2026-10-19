# pghacker-mcp evaluation: reviews as their authors would give them

5 of 10 reviews passed (judged to raise the reviewer's main point, with valid verbatim citations, at least 2 in the reviewer's own words, and none sent on or after the patch's date or from the actual review).
Mean score, the share of the reviewer's actual points raised: 0.55.
Skill: none (the baseline).
Answer model: `github-copilot:gpt-6-luna`; judge: `github-copilot:gpt-6-luna`; run 2026-10-16T05:28:19.199Z.

pghacker-mcp: the endpoint given as `--mcp-url`.

| Review | Reviewer | Correct | Score | Citations valid | Reviewer's own | No leak | Seconds |
| --- | --- | --- | --- | --- | --- | --- | --- |
| r01: [PATCH] Add ALTER SYSTEM RELOAD | Tom Lane | no | 0.00 | yes | 2 | yes | 97 |
| r02: [PATCH] intXshr, intXshl: return error on shift count out of range | Tom Lane | yes | 1.00 | yes | 2 | yes | 56 |
| r03: find_all_inheritors() with NoLock fails on concurrently dropped sub-partitions | Tom Lane | no | 0.00 | yes | 3 | yes | 82 |
| r04: [PATCH] bufmgr: tighten LWLock:BufferMapping on InvalidateBuffer | Andres Freund | yes | 1.00 | yes | 3 | yes | 157 |
| r05: Show effective xmin in pg_replication_slots when xmin is not set | Andres Freund | yes | 1.00 | yes | 3 | yes | 132 |
| r06: Costing for parallel scans with few/single row produced in the outer side | Robert Haas | no | 0.00 | yes | 3 | yes | 126 |
| r07: pg_resetwal: refuse to run when backup_label exists | Michael Paquier | no | 0.00 | yes | 2 | yes | 96 |
| r08: Re: pg_upgrade --copy-file-range fails with EINVAL on Linux 4.19 | Peter Eisentraut | yes | 1.00 | yes | 2 | yes | 94 |
| r09: Opportunistic pruning is lost under direct io, and nothing shows it | Melanie Plageman | no | 0.50 | yes | 4 | yes | 142 |
| r10: [PATCH] Add ALTER SYSTEM RELOAD | Chao Li | yes | 1.00 | yes | 2 | yes | 163 |

Notes: the baseline for the first 10 reviews of [RESULTS-reviews.md](RESULTS-reviews.md), the same reviews without the skill. A second run passed 6 of 10, with the same mean score.
