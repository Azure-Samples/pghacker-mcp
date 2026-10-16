// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! How a client's tool calls group into the questions its user asks, by the
//! pauses between them. A call that comes more than the question gap after
//! the client's last call ended starts a new question; while a call runs,
//! the next one belongs to the same question. A gap under the session gap
//! keeps the session active. A client is an address and an app, followed in
//! memory: a restart forgets them all.

use std::collections::HashMap;

/// Clients followed at most. Beyond, the idle ones are forgotten, and while
/// none is idle a new client isn't followed.
pub(super) const MAX_CLIENTS: usize = 100_000;

/// A client: the usage identity's user, a keyed hash of the address, and
/// the app's `clientInfo` name, in lowercase.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Key {
    pub user: String,
    pub app: String,
}

/// A question: its number for the client, when its first call arrived and
/// its last call ended (unix milliseconds), its calls, and how many failed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Question {
    pub number: u32,
    pub start: u64,
    pub end: u64,
    pub calls: u32,
    pub failed: u32,
}

impl Question {
    fn new(number: u32, now: u64) -> Self {
        Self {
            number,
            start: now,
            end: now,
            ..Self::default()
        }
    }

    /// Whether one of its calls succeeded.
    pub(super) fn succeeded(&self) -> bool {
        self.calls > self.failed
    }
}

/// How a call arrives.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Arrival {
    /// Not followed: too many clients are.
    Untracked,
    /// Part of the current question, `number`, after a gap since the
    /// client's last call ended (milliseconds; 0 while one runs).
    Same { number: u32, gap: u64 },
    /// The start of question `number`, after the question before it, if
    /// any, and the gap since that one's last call ended (milliseconds).
    New {
        number: u32,
        previous: Option<(Question, u64)>,
    },
}

struct Client {
    /// When its latest call ended, or the latest one arrived.
    last: u64,
    /// Its calls running.
    running: u32,
    question: Question,
}

#[derive(Default)]
pub(super) struct Questions {
    clients: HashMap<Key, Client>,
    /// Set when the table is full and a search for idle clients to forget
    /// found none: the earliest time (unix milliseconds) that any of them
    /// could become idle. Until then, a new client isn't followed, and the
    /// table isn't searched again.
    next_sweep: u64,
}

impl Questions {
    /// A call of `key`'s arriving at `now`. A gap over `question_gap` starts
    /// a question; the client's first call starts its first. `session_gap`
    /// says who is idle, should room be needed.
    pub(super) fn arrive(
        &mut self,
        key: &Key,
        now: u64,
        question_gap: u64,
        session_gap: u64,
    ) -> Arrival {
        if !self.room(key, now, session_gap) {
            return Arrival::Untracked;
        }
        let Some(client) = self.clients.get_mut(key) else {
            self.clients.insert(
                key.clone(),
                Client {
                    last: now,
                    running: 1,
                    question: Question::new(1, now),
                },
            );
            return Arrival::New {
                number: 1,
                previous: None,
            };
        };
        let gap = if client.running > 0 {
            0
        } else {
            now.saturating_sub(client.last)
        };
        client.running += 1;
        client.last = client.last.max(now);
        if gap <= question_gap {
            return Arrival::Same {
                number: client.question.number,
                gap,
            };
        }
        let previous = client.question;
        let number = previous.number.saturating_add(1);
        client.question = Question::new(number, now);
        Arrival::New {
            number,
            previous: Some((previous, gap)),
        }
    }

    /// A retry of `key`'s carrying its user's answer, at `now`: it belongs to
    /// the current question, however long the user took. Whether it's
    /// followed: a client unknown here, as after a restart, starts its first
    /// question.
    pub(super) fn resume(&mut self, key: &Key, now: u64, session_gap: u64) -> bool {
        if !self.room(key, now, session_gap) {
            return false;
        }
        let client = self.clients.entry(key.clone()).or_insert_with(|| Client {
            last: now,
            running: 0,
            question: Question::new(1, now),
        });
        client.running += 1;
        client.last = client.last.max(now);
        true
    }

    /// A call of `key`'s that arrived but doesn't run: it's answered with
    /// the form, and its retry will run.
    pub(super) fn hold(&mut self, key: &Key, now: u64) {
        if let Some(client) = self.clients.get_mut(key) {
            client.running = client.running.saturating_sub(1);
            client.last = client.last.max(now);
        }
    }

    /// A call of `key`'s that ended at `now`, `ok` or failed.
    pub(super) fn finish(&mut self, key: &Key, ok: bool, now: u64) {
        if let Some(client) = self.clients.get_mut(key) {
            client.running = client.running.saturating_sub(1);
            client.last = client.last.max(now);
            let question = &mut client.question;
            question.calls = question.calls.saturating_add(1);
            if !ok {
                question.failed = question.failed.saturating_add(1);
            }
            question.end = question.end.max(now);
        }
    }

    /// Whether `key` is or can be followed at `now`. When the table is full,
    /// it forgets the clients idle for `session_gap` to make room. If none
    /// is idle, it refuses new clients without searching again until one
    /// could be (`next_sweep`), so that a flood of new clients doesn't make
    /// every call search the whole table.
    fn room(&mut self, key: &Key, now: u64, session_gap: u64) -> bool {
        if self.clients.len() < MAX_CLIENTS || self.clients.contains_key(key) {
            return true;
        }
        if now < self.next_sweep {
            return false;
        }
        // A running client goes idle a session gap after it ends, at the
        // soonest.
        let mut soonest = now.saturating_add(session_gap);
        self.clients.retain(|_, client| {
            let idle_at = client.last.saturating_add(session_gap);
            let keep = client.running > 0 || now < idle_at;
            if keep && client.running == 0 {
                soonest = soonest.min(idle_at);
            }
            keep
        });
        if self.clients.len() < MAX_CLIENTS {
            return true;
        }
        self.next_sweep = soonest;
        false
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.clients.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUESTION: u64 = 180_000;
    const SESSION: u64 = 600_000;
    const MIN: u64 = 60_000;

    fn key(user: &str, app: &str) -> Key {
        Key {
            user: user.to_owned(),
            app: app.to_owned(),
        }
    }

    fn arrive(questions: &mut Questions, key: &Key, now: u64) -> Arrival {
        questions.arrive(key, now, QUESTION, SESSION)
    }

    /// A call of `key`'s at `now` that takes a second and ends `ok`.
    fn call(questions: &mut Questions, key: &Key, now: u64, ok: bool) -> Arrival {
        let arrival = arrive(questions, key, now);
        questions.finish(key, ok, now + 1_000);
        arrival
    }

    #[test]
    fn a_gap_over_the_question_gap_starts_a_question() {
        let mut questions = Questions::default();
        let a = key("u", "copilot-cli");
        assert_eq!(
            call(&mut questions, &a, 0, true),
            Arrival::New {
                number: 1,
                previous: None
            }
        );
        // Up to the question gap after the last call ended, the same question.
        assert_eq!(
            call(&mut questions, &a, 5_000, true),
            Arrival::Same {
                number: 1,
                gap: 4_000
            }
        );
        assert_eq!(
            call(&mut questions, &a, 6_000 + QUESTION, false),
            Arrival::Same {
                number: 1,
                gap: QUESTION
            }
        );
        // Beyond it, the next, after the one before: its calls, the failure
        // among them, and the gap.
        let start = 7_000 + 2 * QUESTION + 1;
        let Arrival::New {
            number,
            previous: Some((previous, gap)),
        } = call(&mut questions, &a, start, true)
        else {
            panic!("a new question");
        };
        assert_eq!(number, 2);
        assert_eq!(gap, QUESTION + 1);
        assert_eq!(
            previous,
            Question {
                number: 1,
                start: 0,
                end: 7_000 + QUESTION,
                calls: 3,
                failed: 1,
            }
        );
        assert!(previous.succeeded());
    }

    #[test]
    fn while_a_call_runs_nothing_new_starts() {
        let mut questions = Questions::default();
        let a = key("u", "codex-mcp-client");
        arrive(&mut questions, &a, 0);
        // A long call: the next arrives long after the first arrived, but
        // while it runs.
        assert_eq!(
            arrive(&mut questions, &a, 10 * MIN),
            Arrival::Same { number: 1, gap: 0 }
        );
        questions.finish(&a, true, 11 * MIN);
        questions.finish(&a, true, 12 * MIN);
        // The gap runs from the end of the last call to end.
        assert_eq!(
            arrive(&mut questions, &a, 12 * MIN + QUESTION),
            Arrival::Same {
                number: 1,
                gap: QUESTION
            }
        );
        questions.finish(&a, true, 16 * MIN);
        let Arrival::New {
            previous: Some((previous, gap)),
            ..
        } = arrive(&mut questions, &a, 16 * MIN + QUESTION + 1)
        else {
            panic!("a new question");
        };
        assert_eq!(gap, QUESTION + 1);
        assert_eq!(previous.calls, 3);
    }

    #[test]
    fn apps_and_addresses_are_followed_apart() {
        let mut questions = Questions::default();
        let copilot = key("u", "copilot-cli");
        let codex = key("u", "codex-mcp-client");
        let elsewhere = key("v", "copilot-cli");
        call(&mut questions, &copilot, 0, true);
        // Another app's calls, or another address's, don't keep a client's
        // question going.
        call(&mut questions, &codex, 2 * MIN, true);
        call(&mut questions, &elsewhere, 4 * MIN, true);
        assert!(matches!(
            call(&mut questions, &copilot, 5 * MIN, true),
            Arrival::New { number: 2, .. }
        ));
        assert!(matches!(
            call(&mut questions, &codex, 6 * MIN, true),
            Arrival::New { number: 2, .. }
        ));
    }

    #[test]
    fn a_held_call_counts_no_call_and_its_retry_belongs_to_the_question() {
        let mut questions = Questions::default();
        let a = key("u", "copilot-cli");
        call(&mut questions, &a, 0, true);
        // The second question's first call is answered with the form.
        assert!(matches!(
            arrive(&mut questions, &a, 5 * MIN),
            Arrival::New { number: 2, .. }
        ));
        questions.hold(&a, 5 * MIN);
        // The user takes 8 minutes; the retry runs as part of question 2.
        assert!(questions.resume(&a, 13 * MIN, SESSION));
        questions.finish(&a, true, 13 * MIN + 1_000);
        assert_eq!(
            call(&mut questions, &a, 14 * MIN, true),
            Arrival::Same {
                number: 2,
                gap: MIN - 1_000
            }
        );
        let Arrival::New {
            number: 3,
            previous: Some((previous, _)),
        } = call(&mut questions, &a, 20 * MIN, true)
        else {
            panic!("question 3");
        };
        assert_eq!((previous.number, previous.calls), (2, 2));
        assert_eq!(previous.start, 5 * MIN);
        // A retry from a client unknown here, as after a restart, starts its
        // first question.
        let b = key("w", "copilot-cli");
        assert!(questions.resume(&b, 0, SESSION));
        questions.finish(&b, true, 1_000);
        assert_eq!(
            call(&mut questions, &b, 2_000, true),
            Arrival::Same {
                number: 1,
                gap: 1_000
            }
        );
    }

    #[test]
    fn a_question_of_failures_did_not_succeed() {
        let mut questions = Questions::default();
        let a = key("u", "copilot-cli");
        call(&mut questions, &a, 0, false);
        call(&mut questions, &a, 1_000, false);
        let Arrival::New {
            previous: Some((previous, _)),
            ..
        } = call(&mut questions, &a, 10 * MIN, true)
        else {
            panic!("a new question");
        };
        assert_eq!((previous.calls, previous.failed), (2, 2));
        assert!(!previous.succeeded());
        assert!(!Question::default().succeeded());
    }

    #[test]
    fn beyond_its_bound_it_forgets_the_idle_and_follows_no_newcomer() {
        let mut questions = Questions::default();
        for n in 0..MAX_CLIENTS {
            arrive(&mut questions, &key(&n.to_string(), "copilot-cli"), 0);
        }
        // All running: no room.
        let newcomer = key("new", "copilot-cli");
        assert_eq!(arrive(&mut questions, &newcomer, 1), Arrival::Untracked);
        assert!(!questions.resume(&newcomer, 1, SESSION));
        // A known client still is followed.
        assert_eq!(
            arrive(&mut questions, &key("0", "copilot-cli"), 2),
            Arrival::Same { number: 1, gap: 0 }
        );
        // Once they end and the session gap passes, they're forgotten.
        for n in 0..MAX_CLIENTS {
            questions.finish(&key(&n.to_string(), "copilot-cli"), true, 10);
        }
        questions.finish(&key("0", "copilot-cli"), true, 10);
        assert!(matches!(
            arrive(&mut questions, &newcomer, 10 + SESSION),
            Arrival::New { number: 1, .. }
        ));
        assert_eq!(questions.len(), 1);
    }

    #[test]
    fn when_full_it_looks_for_room_again_only_once_a_client_could_be_idle() {
        let mut questions = Questions::default();
        for n in 0..MAX_CLIENTS {
            let client = key(&n.to_string(), "copilot-cli");
            arrive(&mut questions, &client, 0);
            // The last ends a minute later than the rest.
            questions.finish(&client, true, if n == 7 { MIN } else { 0 });
        }
        let newcomer = key("new", "copilot-cli");
        // None idle: no room, and none until the first of them could be.
        assert_eq!(arrive(&mut questions, &newcomer, 1), Arrival::Untracked);
        assert_eq!(questions.next_sweep, SESSION);
        assert_eq!(
            arrive(&mut questions, &newcomer, SESSION - 1),
            Arrival::Untracked
        );
        // Then all but the one that ended later are.
        assert!(matches!(
            arrive(&mut questions, &newcomer, SESSION),
            Arrival::New { number: 1, .. }
        ));
        assert_eq!(questions.len(), 2);
    }
}
