# pghacker-mcp evaluation: reviews as their authors would give them

7 of 10 reviews passed (judged to raise the reviewer's main point, with valid verbatim citations, at least 2 in the reviewer's own words, and none sent on or after the patch's date or from the actual review).
Mean score, the share of the reviewer's actual points raised: 0.75.
Skill: `skills/review-as-author/SKILL.md`.
Answer model: `github-copilot:gpt-6-luna`; judge: `github-copilot:gpt-6-luna`; run 2026-10-16T05:57:46.140Z.

pghacker-mcp: the endpoint given as `--mcp-url`.

| Review | Reviewer | Correct | Score | Citations valid | Reviewer's own | No leak | Seconds |
| --- | --- | --- | --- | --- | --- | --- | --- |
| r01: [PATCH] Add ALTER SYSTEM RELOAD | Tom Lane | yes | 1.00 | yes | 4 | yes | 124 |
| r02: [PATCH] intXshr, intXshl: return error on shift count out of range | Tom Lane | yes | 1.00 | yes | 2 | yes | 103 |
| r03: find_all_inheritors() with NoLock fails on concurrently dropped sub-partitions | Tom Lane | yes | 1.00 | yes | 3 | yes | 166 |
| r04: [PATCH] bufmgr: tighten LWLock:BufferMapping on InvalidateBuffer | Andres Freund | yes | 1.00 | yes | 2 | yes | 129 |
| r05: Show effective xmin in pg_replication_slots when xmin is not set | Andres Freund | no | 0.50 | yes | 2 | yes | 183 |
| r06: Costing for parallel scans with few/single row produced in the outer side | Robert Haas | yes | 0.50 | yes | 4 | yes | 134 |
| r07: pg_resetwal: refuse to run when backup_label exists | Michael Paquier | no | 0.00 | yes | 3 | yes | 102 |
| r08: Re: pg_upgrade --copy-file-range fails with EINVAL on Linux 4.19 | Peter Eisentraut | yes | 1.00 | yes | 3 | yes | 82 |
| r09: Opportunistic pruning is lost under direct io, and nothing shows it | Melanie Plageman | no | 0.50 | yes | 3 | yes | 192 |
| r10: [PATCH] Add ALTER SYSTEM RELOAD | Chao Li | yes | 1.00 | yes | 3 | yes | 140 |

Notes:

- Run before production served `author_perspective`'s `paths` and `until`, against the production endpoint, which ignores them: the as-of date rested on the prompt and the skill. No review cited a message sent on or after its patch's date.
- Without the skill, the same agent passed 5 and 6 of 10 in two runs, with a mean score of 0.55 both times ([RESULTS-reviews-baseline.md](RESULTS-reviews-baseline.md) is the first). The skill's first version passed 7 of 10, with a mean score of 0.60. In r05 to r07 its reviews listed details where the reviewers had rejected the premise, which led to the skill's first step of the review: the verdict on the premise and the approach.
- Ten reviews are a small sample: from one run to the next, a review can pass or fail.
- r01 and r10 are the same patch, as Tom Lane and as Chao Li would review it: each review raised its own reviewer's actual point.
