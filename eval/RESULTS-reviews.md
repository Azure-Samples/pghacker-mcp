# pghacker-mcp evaluation: reviews as their authors would give them

5 of 14 reviews passed (judged to raise the reviewer's main point, with valid verbatim citations, at least 2 in the reviewer's own words, and none sent on or after the patch's date or from the actual review).
Mean score, the share of the reviewer's actual points raised: 0.52.
Skill: `skill://review-as-author/SKILL.md` in the prompt.
Answer model: `github-copilot:gpt-6-luna`; judge: `github-copilot:gpt-6-luna`; run 2026-10-18T23:59:15.126Z.

pghacker-mcp: the endpoint given as `--mcp-url`.

| Review | Reviewer | Correct | Score | Citations valid | Reviewer's own | No leak | Loaded the skill | Seconds |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| r01: [PATCH] Add ALTER SYSTEM RELOAD | Tom Lane | yes | 0.50 | yes | 4 | yes | yes | 69 |
| r02: [PATCH] intXshr, intXshl: return error on shift count out of range | Tom Lane | yes | 1.00 | yes | 2 | yes | yes | 78 |
| r03: find_all_inheritors() with NoLock fails on concurrently dropped sub-partitions | Tom Lane | no | 1.00 | yes | 3 | yes | yes | 87 |
| r04: [PATCH] bufmgr: tighten LWLock:BufferMapping on InvalidateBuffer | Andres Freund | yes | 1.00 | yes | 3 | yes | yes | 152 |
| r05: Show effective xmin in pg_replication_slots when xmin is not set | Andres Freund | no | 0.00 | yes | 2 | yes | yes | 87 |
| r06: Costing for parallel scans with few/single row produced in the outer side | Robert Haas | no | 0.00 | yes | 4 | yes | yes | 526 |
| r07: pg_resetwal: refuse to run when backup_label exists | Michael Paquier | no | 0.00 | yes | 4 | yes | yes | 71 |
| r08: Re: pg_upgrade --copy-file-range fails with EINVAL on Linux 4.19 | Peter Eisentraut | yes | 1.00 | yes | 3 | yes | yes | 133 |
| r09: Opportunistic pruning is lost under direct io, and nothing shows it | Melanie Plageman | no | 0.50 | yes | 3 | yes | no | 118 |
| r10: [PATCH] Add ALTER SYSTEM RELOAD | Chao Li | no | 0.00 | yes | 3 | yes | yes | 139 |
| r11: Re: Wrong results from an antijoin | Tom Lane | no | 0.50 | yes | 4 | yes | yes | 117 |
| r12: Memory leak in statext_ndistinct_build() during ANALYZE | Michael Paquier | no | 0.33 | yes | 3 | yes | yes | 72 |
| r13: Reset waitStart when a lock wait fails | Chao Li | yes | 1.00 | yes | 1 | yes | yes | 43 |
| r14: [PATCH] CI: Add a CPAN cache on Windows | Andres Freund | yes | 0.50 | yes | 4 | yes | yes | 105 |

Notes:

- The skill as deployed, version 1, on the 14 reviews: the 10 whose reviewers objected to the patch and the 4 whose reviewers welcomed it (r11 to r14). Run against production with the skill in the prompt; the agents' calls of `load_skill_review_as_author` returned the same skill.
- Over two runs of each review (this one, the same day's run of r01 to r10 noted in [RESULTS-reviews-tool.md](RESULTS-reviews-tool.md), and a second run of r11 to r14), 11 of the 20 reviews of patches their reviewers objected to passed, with a mean score of 0.57, and 4 of the 8 of patches they welcomed, with 0.63. 6 of those 8 were judged correct: r13 failed the citation check in every run, with only one quote in Chao Li's own words.
- Two drafts of the skill's verdict step, meant to catch more of the reviewers' objections, didn't do better, so the skill stays as it was. Each ran with the draft both in the prompt and from `load_skill_review_as_author`, which a local proxy to production answered with the draft:
  - A separated the problem from the fix, made the case against the change first, and gave the change the burden of proof. In one run, 6 of 10 and 1 of 4 passed, with mean scores of 0.50 and 0.46. Its reviews often predicted that the reviewer would ask for a stronger case, where the reviewer had agreed or objected outright.
  - B separated the problem from the fix, weighed both cases, committed to one of four verdicts, and stated an objection as one. In two runs, 10 of 20 and 4 of 8 passed, with 0.47 and 0.67.
- The same skill passed 7 and 4 of the 10 objected-to reviews in two runs the same day: a difference of a few reviews between versions is noise.
- r05 failed in every run: Andres Freund's objection, to showing a computed value in an existing column, has no precedent that search finds in his messages before the patch.
- On the 10 objected-to reviews alone, before production served `author_perspective`'s `paths` and `until`, the skill passed 7 of 10, with a mean score of 0.75; without it, 5 and 6 of 10 passed ([RESULTS-reviews-baseline.md](RESULTS-reviews-baseline.md)).
- r01 and r10 are the same patch, as Tom Lane and as Chao Li would review it.
