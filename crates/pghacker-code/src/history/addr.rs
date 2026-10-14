//! Commit addresses: int8 labels in pre-order that are never renumbered.
//!
//! Master commit k (the root is 0) gets k·2³². The n-th branch forked at
//! master commit f owns the block [f·2³² + n·2²⁴ + 1, f·2³² + (n+1)·2²⁴]; its
//! j-th commit (from 1) gets the block's start + j − 1. So the descendants of
//! a master commit are every address from its own on, and the descendants of
//! a branch commit run from it to the end of its block.

/// The gap between consecutive master commits.
pub const MASTER_STEP: i64 = 1 << 32;
/// The addresses a branch's block holds.
pub const BRANCH_BLOCK: i64 = 1 << 24;
/// Branches that can fork at one master commit: the last block must end
/// before the next master commit's address.
pub const MAX_BRANCHES_PER_FORK: u32 = (MASTER_STEP / BRANCH_BLOCK) as u32 - 1;
/// An unbounded upper end: alive at every later master commit.
pub const OPEN: i64 = i64::MAX;

/// Master commit `k`'s address.
pub fn master(k: u32) -> i64 {
    i64::from(k) * MASTER_STEP
}

/// Whether `addr` is a master commit's.
pub fn is_master(addr: i64) -> bool {
    addr % MASTER_STEP == 0
}

/// The master commit index of `addr`'s fork point (its own for master).
pub fn fork_of(addr: i64) -> u32 {
    (addr / MASTER_STEP) as u32
}

/// The first address and the end (exclusive) of the `n`-th branch forked
/// at master commit `fork`.
pub fn branch_block(fork: u32, n: u32) -> (i64, i64) {
    assert!(
        n < MAX_BRANCHES_PER_FORK,
        "more than {MAX_BRANCHES_PER_FORK} branches fork at one commit"
    );
    let base = master(fork) + i64::from(n) * BRANCH_BLOCK;
    (base + 1, base + BRANCH_BLOCK + 1)
}

/// The descendants of `addr` (itself included), as [lo, hi).
pub fn descendants(addr: i64) -> (i64, i64) {
    if is_master(addr) {
        return (addr, OPEN);
    }
    let offset = addr % MASTER_STEP;
    let n = (offset - 1) / BRANCH_BLOCK;
    let base = addr - offset;
    (addr, base + (n + 1) * BRANCH_BLOCK + 1)
}

/// Disjoint, sorted [lo, hi) ranges: where a line or file version is alive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alive(pub Vec<(i64, i64)>);

impl Alive {
    /// Alive from `born` on, minus the descendants of each death.
    pub fn of(born: i64, deaths: impl IntoIterator<Item = i64>) -> Self {
        let mut alive = Alive(vec![descendants(born)]);
        for death in deaths {
            alive.subtract(descendants(death));
        }
        alive
    }

    fn subtract(&mut self, (lo, hi): (i64, i64)) {
        let mut out = Vec::with_capacity(self.0.len() + 1);
        for &(a, b) in &self.0 {
            if b <= lo || a >= hi {
                out.push((a, b));
                continue;
            }
            if a < lo {
                out.push((a, lo));
            }
            if b > hi {
                out.push((hi, b));
            }
        }
        self.0 = out;
    }

    pub fn contains(&self, addr: i64) -> bool {
        self.0.iter().any(|&(a, b)| a <= addr && addr < b)
    }

    /// The smallest range holding every address: [lo, hi).
    pub fn hull(&self) -> (i64, i64) {
        match (self.0.first(), self.0.last()) {
            (Some(first), Some(last)) => (first.0, last.1),
            _ => (0, 0),
        }
    }

    /// Whether the hull has holes, so the ranges must be stored.
    pub fn has_holes(&self) -> bool {
        self.0.len() > 1
    }

    /// PostgreSQL's int8multirange text form.
    pub fn to_sql(&self) -> String {
        let ranges: Vec<String> = self
            .0
            .iter()
            .map(|&(a, b)| {
                if b == OPEN {
                    format!("[{a},)")
                } else {
                    format!("[{a},{b})")
                }
            })
            .collect();
        format!("{{{}}}", ranges.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_master_and_branch_commits() {
        assert_eq!(master(0), 0);
        assert_eq!(master(3), 3 << 32);
        assert!(is_master(master(7)));
        let (lo, hi) = branch_block(5, 0);
        assert_eq!(lo, master(5) + 1);
        assert_eq!(hi, master(5) + BRANCH_BLOCK + 1);
        assert!(!is_master(lo));
        assert_eq!(fork_of(lo), 5);
        let (lo2, hi2) = branch_block(5, 1);
        assert_eq!(lo2, hi);
        assert!(hi2 < master(6));
        let (_, last) = branch_block(5, MAX_BRANCHES_PER_FORK - 1);
        assert!(last <= master(6));
    }

    #[test]
    fn descendants_of_master_and_branch_commits() {
        assert_eq!(descendants(master(4)), (master(4), OPEN));
        let (lo, hi) = branch_block(4, 1);
        assert_eq!(descendants(lo), (lo, hi));
        assert_eq!(descendants(lo + 10), (lo + 10, hi));
        assert_eq!(descendants(hi - 1), (hi - 1, hi));
    }

    #[test]
    fn alive_subtracts_deaths() {
        let (b_lo, b_hi) = branch_block(3, 0);
        // Born on master 1, deleted on a branch forked at 3, then on master 6.
        let alive = Alive::of(master(1), [b_lo + 2, master(6)]);
        assert_eq!(alive, Alive(vec![(master(1), b_lo + 2), (b_hi, master(6))]));
        assert!(alive.contains(master(2)));
        assert!(alive.contains(b_lo + 1));
        assert!(!alive.contains(b_lo + 2));
        assert!(!alive.contains(b_hi - 1));
        assert!(alive.contains(master(5)));
        assert!(!alive.contains(master(6)));
        assert!(alive.has_holes());
        assert_eq!(alive.hull(), (master(1), master(6)));
        assert_eq!(
            Alive::of(master(2), []).to_sql(),
            format!("{{[{},)}}", master(2))
        );
        // Born and deleted on one branch.
        let born = Alive::of(b_lo + 1, [b_lo + 4]);
        assert_eq!(born, Alive(vec![(b_lo + 1, b_lo + 4)]));
        assert!(!born.has_holes());
    }
}
