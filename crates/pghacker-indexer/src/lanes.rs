// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The lanes extraction leases threads from. A thread needs extraction when
//! extraction never read it, or read it before its latest messages came.
//! Those whose latest message is from the last days (7 by default,
//! `PGHACKER_EXTRACTION_NEW_MAIL_DAYS`) are new mail, leased oldest first, as
//! mail arrives. All the others are the backfill, leased from the newest
//! back. When both lanes have threads to lease, new mail gets at most a share
//! of them (half by default, `PGHACKER_EXTRACTION_NEW_MAIL_SHARE`); a lane
//! without threads leaves the other every slot.

use std::str::FromStr;
use std::sync::Mutex;

use anyhow::{Result, bail};

/// SQL: thread `t` needs extraction. Extraction never read it, or read it
/// (or skipped it) before its latest messages came. A thread without
/// messages waits for some.
pub const NEEDS_EXTRACTION: &str = "t.message_count > 0
     AND (t.extraction_state = 'pending'
          OR (t.extraction_state IN ('extracted', 'skipped') AND t.extracted_version < t.content_version))";

/// SQL: thread `t` is new mail: its latest message is from the last `days`
/// days, `days` being a parameter such as `$2`.
pub fn new_mail(days: &str) -> String {
    format!("t.last_at >= now() - make_interval(days => {days})")
}

/// SQL: thread `t` is in the backfill: its latest message is older than
/// `days` days, or it has no date. Exactly the threads that aren't new mail.
pub fn backfill(days: &str) -> String {
    format!("(t.last_at < now() - make_interval(days => {days}) OR t.last_at IS NULL)")
}

/// The order next_extraction_batch leases threads in:
/// `PGHACKER_EXTRACTION_ORDER`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Order {
    /// New mail and the backfill.
    #[default]
    Lanes,
    /// The threads never extracted first, the largest first, then the ones
    /// that changed: the order before the lanes.
    Largest,
}

impl Order {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lanes => "lanes",
            Self::Largest => "largest",
        }
    }
}

impl FromStr for Order {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim() {
            "lanes" => Ok(Self::Lanes),
            "largest" => Ok(Self::Largest),
            other => bail!("PGHACKER_EXTRACTION_ORDER must be lanes or largest, not {other:?}"),
        }
    }
}

/// Where a leased thread came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    NewMail,
    Backfill,
    /// Leased by its id.
    Requested,
    /// Leased in the `largest` order.
    Largest,
}

impl Lane {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NewMail => "new_mail",
            Self::Backfill => "backfill",
            Self::Requested => "requested",
            Self::Largest => "largest",
        }
    }
}

/// New mail's credit, and the threads each lane has leased, since the
/// indexer started. Leases are taken one call at a time, under the leases'
/// advisory lock.
#[derive(Debug, Default)]
pub struct Lanes {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// From 0 to below 1: each slot that both lanes want adds the share, and
    /// new mail takes the slot once its credit reaches 1.
    credit: f64,
    new_mail: u64,
    backfill: u64,
}

/// Slack for shares whose sums reach 1 only in decimal, such as 0.1 ten times.
const SLACK: f64 = 1e-9;

impl Lanes {
    /// Up to `count` threads from the lanes' heads, each given in its lane's
    /// order, with the lane each came from. A slot goes to the only lane with
    /// a thread left; when both have one, new mail takes the slot once its
    /// credit reaches 1. So while both lanes have threads, new mail gets at
    /// most `share` of the slots, the backfill the first one; and a share
    /// that new mail doesn't use isn't kept for later, so a burst of new mail
    /// gets no more.
    pub fn pick<T>(
        &self,
        count: usize,
        share: f64,
        new_mail: Vec<T>,
        backfill: Vec<T>,
    ) -> Vec<(T, Lane)> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut new_mail = new_mail.into_iter().peekable();
        let mut backfill = backfill.into_iter().peekable();
        let mut picked = Vec::new();
        while picked.len() < count {
            let lane = match (new_mail.peek().is_some(), backfill.peek().is_some()) {
                (false, false) => break,
                (true, false) => Lane::NewMail,
                (false, true) => Lane::Backfill,
                (true, true) => {
                    state.credit += share;
                    if state.credit >= 1.0 - SLACK {
                        state.credit = (state.credit - 1.0).max(0.0);
                        Lane::NewMail
                    } else {
                        Lane::Backfill
                    }
                }
            };
            let thread = if lane == Lane::NewMail {
                state.new_mail += 1;
                new_mail.next()
            } else {
                state.backfill += 1;
                backfill.next()
            };
            if let Some(thread) = thread {
                picked.push((thread, lane));
            }
        }
        picked
    }

    /// Threads leased from new mail and from the backfill since the indexer
    /// started.
    pub fn leased(&self) -> (u64, u64) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.new_mail, state.backfill)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lanes of `slots` single-thread picks with both lanes always full.
    fn contended(lanes: &Lanes, share: f64, slots: usize) -> Vec<Lane> {
        (0..slots)
            .map(|_| lanes.pick(1, share, vec![1], vec![2])[0].1)
            .collect()
    }

    #[test]
    fn contended_slots_alternate_backfill_first() {
        use Lane::{Backfill, NewMail};
        let lanes = Lanes::default();
        assert_eq!(
            contended(&lanes, 0.5, 6),
            [Backfill, NewMail, Backfill, NewMail, Backfill, NewMail]
        );
        assert_eq!(lanes.leased(), (3, 3));
        let three = Lanes::default();
        let picked = three.pick(3, 0.5, vec![1, 2, 3], vec![4, 5, 6]);
        assert_eq!(picked, [(4, Backfill), (1, NewMail), (5, Backfill)]);
        let picked = three.pick(3, 0.5, vec![2, 3], vec![5, 6]);
        assert_eq!(picked, [(2, NewMail), (5, Backfill), (3, NewMail)]);
    }

    #[test]
    fn new_mail_never_exceeds_its_share() {
        for share in [0.1, 0.25, 0.3, 0.5, 0.7] {
            let lanes = Lanes::default();
            let mut new_mail = 0;
            for (slot, lane) in contended(&lanes, share, 200).into_iter().enumerate() {
                new_mail += usize::from(lane == Lane::NewMail);
                let slots = slot + 1;
                assert!(
                    new_mail as f64 <= share * slots as f64 + SLACK,
                    "share {share}: {new_mail} of {slots}"
                );
                assert!(
                    new_mail as f64 > share * slots as f64 - 1.0,
                    "share {share}: only {new_mail} of {slots}"
                );
            }
        }
        let tenth = Lanes::default();
        let lanes = contended(&tenth, 0.1, 10);
        assert_eq!(lanes.last(), Some(&Lane::NewMail), "0.1 ten times is 1");
        assert_eq!(contended(&Lanes::default(), 1.0, 3), [Lane::NewMail; 3]);
    }

    #[test]
    fn a_lane_without_threads_leaves_every_slot_to_the_other() {
        let lanes = Lanes::default();
        let all = lanes.pick(3, 0.5, Vec::<i64>::new(), vec![7, 8, 9]);
        assert_eq!(all.iter().map(|p| p.0).collect::<Vec<_>>(), [7, 8, 9]);
        assert!(all.iter().all(|p| p.1 == Lane::Backfill));
        let all = lanes.pick(3, 0.5, vec![1, 2], Vec::new());
        assert_eq!(all, [(1, Lane::NewMail), (2, Lane::NewMail)]);
        assert!(lanes.pick(3, 0.5, Vec::<i64>::new(), Vec::new()).is_empty());
        // The lanes alone didn't move the credit: contention starts with
        // the backfill.
        assert_eq!(contended(&lanes, 0.5, 2), [Lane::Backfill, Lane::NewMail]);
        assert_eq!(lanes.leased(), (3, 4));
    }

    #[test]
    fn orders_parse() {
        assert_eq!("lanes".parse::<Order>().unwrap(), Order::Lanes);
        assert_eq!(" largest ".parse::<Order>().unwrap(), Order::Largest);
        assert!("newest".parse::<Order>().is_err());
    }
}
