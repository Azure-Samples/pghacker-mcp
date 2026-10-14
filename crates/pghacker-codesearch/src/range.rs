//! Ranges of commits, as git log takes them, over commit addresses: `X` is
//! X and its ancestors; `A..B` is B's ancestors that aren't A's. In
//! PostgreSQL's history, a tree, a range is master commits in an interval
//! and at most one stretch of one branch.

use crate::rev::parent;

/// The step between master commits' addresses.
pub const MASTER_STEP: i64 = 1 << 32;
const BRANCH_BLOCK: i64 = 1 << 24;

/// Commits: master commits with addresses in (`after`, `upto`], and the
/// branch commits in [`from`, `to`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub master: Option<(i64, i64)>,
    pub branch: Option<(i64, i64)>,
}

/// The master commit a commit is on or forks from.
fn fork(addr: i64) -> i64 {
    addr - addr % MASTER_STEP
}

/// The first address of a branch commit's block.
fn block_start(addr: i64) -> i64 {
    let offset = addr % MASTER_STEP;
    addr - offset + (offset - 1) / BRANCH_BLOCK * BRANCH_BLOCK + 1
}

/// The last address of a branch commit's block.
fn block_end(addr: i64) -> i64 {
    block_start(addr) + BRANCH_BLOCK - 1
}

impl Range {
    /// `upto` and its ancestors.
    pub fn ancestors(upto: i64) -> Self {
        Self::between(None, upto)
    }

    /// `upto`'s ancestors that aren't `since`'s (or all, without `since`).
    pub fn between(since: Option<i64>, upto: i64) -> Self {
        let on_master = upto % MASTER_STEP == 0;
        let top = fork(upto);
        // Master commits `since` has: up to its fork point.
        let had = since.map(fork);
        let master = match had {
            Some(h) if h >= top => None,
            Some(h) => Some((h, top)),
            None => Some((-1, top)),
        };
        let branch = if on_master {
            None
        } else {
            let start = block_start(upto);
            match since {
                // `since` on the same branch: only commits after it.
                Some(s) if s % MASTER_STEP != 0 && block_start(s) == start => {
                    (s < upto).then_some((s + 1, upto))
                }
                _ => Some((start, upto)),
            }
        };
        Range { master, branch }
    }

    /// Whether the range has commit `addr`.
    pub fn contains(&self, addr: i64) -> bool {
        if addr % MASTER_STEP == 0 {
            self.master.is_some_and(|(a, b)| a < addr && addr <= b)
        } else {
            self.branch.is_some_and(|(a, b)| a <= addr && addr <= b)
        }
    }

    /// Whether some commit in the range is in [lo, hi).
    pub fn meets(&self, lo: i64, hi: i64) -> bool {
        if let Some((a, b)) = self.master {
            // The first master address from lo on, if past `a`.
            let first = lo.div_euclid(MASTER_STEP) * MASTER_STEP;
            let first = if first < lo {
                first + MASTER_STEP
            } else {
                first
            };
            let first = first.max(a.div_euclid(MASTER_STEP) * MASTER_STEP + MASTER_STEP);
            if first < hi && first <= b {
                return true;
            }
        }
        self.branch.is_some_and(|(a, b)| a.max(lo) <= b.min(hi - 1))
    }

    /// The addresses the range spans, for an index-friendly prefilter.
    pub fn hull(&self) -> (i64, i64) {
        let first_master = |a: i64| a.div_euclid(MASTER_STEP) * MASTER_STEP + MASTER_STEP;
        let lo = [
            self.master.map(|m| first_master(m.0)),
            self.branch.map(|b| b.0),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(0);
        let hi = [self.master.map(|m| m.1), self.branch.map(|b| b.1)]
            .into_iter()
            .flatten()
            .max()
            .unwrap_or(-1);
        (lo, hi + 1)
    }

    /// The first commit in the range in [lo, hi), the earliest address.
    pub fn first_in(&self, lo: i64, hi: i64) -> Option<i64> {
        let mut best: Option<i64> = None;
        if let Some((a, b)) = self.master {
            let first = lo.div_euclid(MASTER_STEP) * MASTER_STEP;
            let first = if first < lo {
                first + MASTER_STEP
            } else {
                first
            };
            let first = first.max(a.div_euclid(MASTER_STEP) * MASTER_STEP + MASTER_STEP);
            if first < hi && first <= b {
                best = Some(first);
            }
        }
        if let Some((a, b)) = self.branch {
            let first = a.max(lo);
            if first <= b.min(hi - 1) {
                best = Some(best.map_or(first, |m| m.min(first)));
            }
        }
        best
    }
}

/// Where a line's alive stretch ends because a commit removed it, rather
/// than at its branch's end: None for a natural end.
pub fn death_at(end: i64) -> Option<i64> {
    if end == i64::MAX {
        return None;
    }
    if end % MASTER_STEP == 0 {
        return Some(end);
    }
    // A stretch ending right after its block's last address ran out.
    let last = end - 1;
    (last % MASTER_STEP == 0 || block_end(last) != last).then_some(end)
}

/// The addresses of a commit's descendants, itself included: on master
/// every later address; on a branch the rest of its block.
pub fn descendants(addr: i64) -> (i64, i64) {
    if addr % MASTER_STEP == 0 {
        (addr, i64::MAX)
    } else {
        (addr, block_end(addr))
    }
}

/// Whether `addr` is a master commit.
pub fn on_master(addr: i64) -> bool {
    addr % MASTER_STEP == 0
}

/// The commit after `at` on its lane, master or `at`'s branch, that removed
/// a line alive at `at` over `spans` (sorted): None while the lane's head has
/// it. On master, holes the line has on branches don't count.
pub fn death_after(spans: &[(i64, i64)], at: i64) -> Option<i64> {
    let covering = |x: i64| {
        spans
            .iter()
            .find(|&&(a, b)| a <= x && x < b)
            .map(|&(_, b)| b)
    };
    let mut end = covering(at)?;
    if at % MASTER_STEP != 0 {
        return (end <= block_end(at)).then_some(end);
    }
    loop {
        if end == i64::MAX {
            return None;
        }
        let next = if end % MASTER_STEP == 0 {
            end
        } else {
            end.div_euclid(MASTER_STEP)
                .checked_add(1)?
                .checked_mul(MASTER_STEP)?
        };
        match covering(next) {
            Some(b) => end = b,
            None => return Some(next),
        }
    }
}

/// Steps back from `addr` `n` times; None past the root.
pub fn back(mut addr: i64, n: u32) -> Option<i64> {
    for _ in 0..n {
        addr = parent(addr)?;
    }
    Some(addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: i64 = MASTER_STEP;

    #[test]
    fn ranges_on_master_and_branches() {
        let all = Range::ancestors(10 * M);
        assert_eq!(all.master, Some((-1, 10 * M)));
        assert!(all.contains(0) && all.contains(10 * M) && !all.contains(11 * M));
        let b = 5 * M + 1;
        let branch = Range::ancestors(b + 3);
        assert_eq!(branch.master, Some((-1, 5 * M)));
        assert_eq!(branch.branch, Some((b, b + 3)));
        assert!(!branch.contains(6 * M) && branch.contains(b + 2));
        let since = Range::between(Some(3 * M), 10 * M);
        assert!(!since.contains(3 * M) && since.contains(4 * M));
        let fixes = Range::between(Some(9 * M), b + 3);
        assert_eq!((fixes.master, fixes.branch), (None, Some((b, b + 3))));
        let later = Range::between(Some(b + 1), b + 3);
        assert_eq!((later.master, later.branch), (None, Some((b + 2, b + 3))));
        let from_branch = Range::between(Some(b + 3), 8 * M);
        assert_eq!(from_branch.master, Some((5 * M, 8 * M)));
    }

    #[test]
    fn deaths_follow_the_lane() {
        let b = 2 * M + 1;
        // Born at 2, changed on its branch at b, removed on master at 4.
        let spans = [(2 * M, b), (2 * M + BRANCH_BLOCK + 1, 4 * M)];
        assert_eq!(death_after(&spans, 2 * M), Some(4 * M));
        assert_eq!(death_after(&spans, 3 * M), Some(4 * M));
        assert_eq!(death_after(&spans, 4 * M), None);
        // On master to the end; on the branch until b + 2.
        let spans = [(2 * M, b + 2), (2 * M + BRANCH_BLOCK + 1, i64::MAX)];
        assert_eq!(death_after(&spans, b), Some(b + 2));
        assert_eq!(death_after(&spans, 2 * M), None);
        // Alive to its branch's end: a natural end.
        assert_eq!(death_after(&[(b, b + BRANCH_BLOCK)], b + 5), None);
    }

    #[test]
    fn meets_finds_master_points_not_branch_addresses() {
        let r = Range::between(Some(2 * M), 6 * M);
        // Alive only on a branch forked at 4: no master commit has it.
        assert!(!r.meets(4 * M + 1, 4 * M + 9));
        assert!(r.meets(4 * M + 1, 5 * M + 1));
        assert!(!r.meets(0, 2 * M + 1));
        assert!(r.meets(0, 3 * M + 1));
        assert!(!r.meets(7 * M, i64::MAX));
        assert_eq!(r.first_in(0, i64::MAX), Some(3 * M));
        let b = 5 * M + 1;
        let br = Range::ancestors(b + 3);
        assert!(br.meets(b + 2, b + 10));
        assert!(!br.meets(b + 4, b + 10));
        assert_eq!(br.first_in(b + 1, b + 10), Some(b + 1));
        assert_eq!(r.hull(), (3 * M, 6 * M + 1));
    }

    #[test]
    fn natural_ends_are_not_deaths() {
        assert_eq!(death_at(i64::MAX), None);
        assert_eq!(death_at(7 * M), Some(7 * M));
        let b = 5 * M + 1;
        assert_eq!(death_at(b + 4), Some(b + 4));
        assert_eq!(death_at(block_end(b) + 1), None);
        assert_eq!(back(7 * M, 2), Some(5 * M));
        assert_eq!(back(b + 1, 2), Some(5 * M));
        assert_eq!(back(0, 1), None);
    }
}
