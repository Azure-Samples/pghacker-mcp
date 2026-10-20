// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! User feedback: now and then, whether the last answer helped, asked
//! through the client's own form (MCP elicitation, carried by the
//! 2026-07-28 protocol's multi round-trip requests) as the user's next
//! question starts.
//!
//! An interactive client, an address and an app whose name contains one on
//! the list, is followed by the pauses between its tool calls
//! ([`questions`]): a gap over the question
//! gap starts a question, and one under the session gap keeps the session
//! active. When a call starts a question in an active session, after the
//! client's first questions and in an hour that the draw arms for the
//! address ([`sampler`]), the server answers the call `input_required` with
//! a form instead of running it ([`form`]), at most once an hour per
//! address. The client asks the user and retries the call with the answer
//! and the form's `requestState`, sealed to the app and the tool ([`token`]).
//! The server records the answer in usage telemetry and runs the call,
//! whatever the answer. The model takes no part.
//!
//! The answer may come from another address than the ask: behind gateways
//! such as Global Secure Access, each new connection may leave from another
//! address of a pool. It still counts, and the hour's ask and a decline
//! count for the address that answered as well as the one asked. The address
//! stays the client's identity otherwise, so such a client's questions are
//! counted per address, and it may be asked again within the hour through a
//! third.
//!
//! What is kept, in memory, follows the clients and keeps the asks rare: per
//! client its questions, and per address the hour last asked, a pause after
//! a decline and recent outcomes; and the asks, with the asked address's /24
//! (IPv6: /48), until they're answered or expire: a task forgets expired asks
//! every minute. A restart forgets it all; the draw doesn't change.
//!
//! Off unless usage telemetry is on and PGHACKER_FEEDBACK_RATE is above 0.
//! A request with no client identity (the internal listener), with
//! `feedback=off` in its URL, from an app whose name contains none on the
//! list, or that can't show a form is never followed or asked.

mod form;
mod questions;
mod sampler;
mod text;
mod token;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, ensure};
use hmac::{Hmac, KeyInit, Mac};
use rmcp::model::{InputRequiredResult, InputResponses};
use sha2::Sha256;
use uuid::Uuid;

use crate::config::Env;
use crate::usage::Usage;
use questions::{Key, Question, Questions};
use sampler::Sampler;
use token::Tokens;

/// No ask for this long after a decline at an address.
const PAUSE: u64 = 24 * 3600;
/// Outcomes recorded at most per address an hour, and per minute in all.
const ADDRESS_OUTCOMES_PER_HOUR: u32 = 3;
const OUTCOMES_PER_MINUTE: u32 = 60;
/// Rejections recorded at most per minute in all; and per address, one.
const REJECTIONS_PER_MINUTE: u32 = 60;
/// Addresses, asks and answered asks kept at most; beyond, the stale are
/// forgotten.
const MAX_ADDRESSES: usize = 100_000;
const MAX_ASKS: usize = 100_000;
const MAX_ANSWERED: usize = 100_000;
/// How often a server forgets the asks whose state has expired.
const SWEEP: Duration = Duration::from_secs(60);

/// The apps asked by default: any whose `clientInfo` name contains one of
/// these, ignoring case. That's Copilot CLI and the GitHub Copilot app
/// (`copilot-cli`), Codex CLI (`codex-mcp-client`), VS Code and VS Code
/// Insiders, Claude Code (`claude-code`) and the Claude app (`claude-ai`),
/// and Cursor (`cursor-vscode`).
pub const CLIENTS: &[&str] = &["copilot", "codex", "Visual Studio Code", "claude", "cursor"];

/// What the environment sets.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// The chance, from 0 to 1, that an address's hour is armed for an ask.
    pub rate: f64,
    /// Whether the form has an optional comment field.
    pub comments: bool,
    /// A gap between calls longer than this starts a question.
    pub question_gap: Duration,
    /// A gap shorter than this keeps the session active.
    pub session_gap: Duration,
    /// The questions a client asks before it can be asked.
    pub min_questions: u32,
    /// The apps that can be asked: any whose `clientInfo` name contains one
    /// of these, ignoring case.
    pub clients: Vec<String>,
    /// For trying the form by hand: every question after a client's first
    /// is asked, whatever the pause before it, the warm-up, the draw, the
    /// hour's ask, a decline or the address's outcomes; and why a call isn't
    /// asked is logged at info.
    pub debug: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            rate: 0.0,
            comments: false,
            question_gap: Duration::from_secs(180),
            session_gap: Duration::from_secs(600),
            min_questions: 3,
            clients: CLIENTS.iter().map(|name| (*name).to_owned()).collect(),
            debug: false,
        }
    }
}

/// Feedback's handle: cheap to clone, and off by default.
#[derive(Clone, Default)]
pub struct Feedback {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    settings: Settings,
    /// The apps that can be asked, in lowercase.
    clients: Vec<String>,
    sampler: Sampler,
    tokens: Tokens,
    state: Mutex<State>,
}

/// What a request tells feedback about its client.
#[derive(Clone, Debug, Default)]
pub struct Caller<'a> {
    /// The usage identity's user, a keyed hash of the client's address.
    /// Without one, as on the internal listener, a client is never asked.
    pub user: Option<&'a str>,
    /// The client's address. An ask keeps its network in memory, to compare
    /// with its answer's; it's never recorded.
    pub address: Option<IpAddr>,
    /// The app's `clientInfo` name.
    pub app: Option<String>,
    /// Whether the request declares form elicitation (MCP 2026-07-28).
    pub elicits: bool,
    /// Whether the request's URL has `feedback=off`.
    pub opted_out: bool,
}

/// What to do with a call that carries no answer to a form.
pub enum Arrival {
    /// Run it.
    Run(Call),
    /// Hold it: answer it with the form, and run its retry.
    Ask(Ask),
}

/// A held call's form, and what it asks about.
#[derive(Debug)]
pub struct Ask {
    /// The `input_required` result that answers the call.
    pub form: InputRequiredResult,
    /// The ask's random id, as 32 hex digits, which names it and its outcome
    /// in usage events.
    pub ask_id: String,
    /// The question it rates.
    pub rated: Rated,
}

/// The question an ask rates: the one before the held call's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rated {
    /// Its number for the client.
    pub number: u32,
    /// Its calls, and how many failed.
    pub calls: u32,
    pub failed: u32,
    /// When its first call arrived and its last call ended.
    pub start: SystemTime,
    pub end: SystemTime,
    /// The pause between it and the held call.
    pub gap: Duration,
}

/// A call that runs, followed until it ends: [`Call::end`], or when it's
/// dropped, as a failure, as when its client goes away.
pub struct Call {
    track: Option<(Arc<Inner>, Key)>,
}

impl Call {
    fn none() -> Self {
        Self { track: None }
    }

    /// The call ended at `now`, `ok` or failed.
    pub fn end(mut self, ok: bool, now: SystemTime) {
        self.finish(ok, now);
    }

    fn finish(&mut self, ok: bool, now: SystemTime) {
        if let Some((inner, key)) = self.track.take() {
            inner.lock().questions.finish(&key, ok, millis(now));
        }
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        self.finish(false, dropped_at());
    }
}

/// When a dropped call ends: now, or in tests, on their clock, the time
/// the test sets.
fn dropped_at() -> SystemTime {
    #[cfg(test)]
    {
        if let Some(at) = tests::DROPPED_AT.get() {
            return at;
        }
    }
    SystemTime::now()
}

/// A retry, which carries the user's answer: the call to run, and the
/// answer to record or why it isn't.
pub struct Resumed {
    pub call: Call,
    pub outcome: Option<Outcome>,
    /// `invalid`, `used` or `limit`: at most once a minute per address.
    pub rejection: Option<&'static str>,
}

/// The user's answer, to record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// The ask's id, as its ask named it.
    pub ask_id: String,
    /// `rated`, `skipped`, `declined` or `dismissed`.
    pub outcome: &'static str,
    /// `up` or `down`, when rated.
    pub rating: Option<&'static str>,
    /// With comments on, what the user wrote.
    pub comment: Option<String>,
    /// Where the answer came from, against the address asked: `same`, the
    /// same /24 (`same24`; for IPv6, the same /48), the same /16 (`same16`;
    /// for IPv6, /32), `other`, or `unknown` when the ask isn't in memory, as
    /// after a restart.
    pub answered_from: &'static str,
}

impl Feedback {
    pub fn off() -> Self {
        Self::default()
    }

    /// Feedback as the environment configures it:
    /// - PGHACKER_FEEDBACK_RATE, the chance from 0 (unset: off) to 1 that an
    ///   address's hour is armed for an ask;
    /// - PGHACKER_FEEDBACK_COMMENTS, whether the form takes a comment;
    /// - PGHACKER_FEEDBACK_QUESTION_GAP and PGHACKER_FEEDBACK_SESSION_GAP,
    ///   in seconds (180 and 600): a longer gap starts a question, a shorter
    ///   one keeps the session active;
    /// - PGHACKER_FEEDBACK_MIN_QUESTIONS (3), the questions before an ask;
    /// - PGHACKER_FEEDBACK_CLIENTS, the apps asked: names separated by
    ///   commas, any of which an app's `clientInfo` name contains, ignoring
    ///   case; unset or blank, [`CLIENTS`];
    /// - PGHACKER_FEEDBACK_DEBUG (false), for trying the form by hand: every
    ///   question after a client's first is asked ([`Settings::debug`]).
    ///
    /// It's on only with usage telemetry, where it records, and its keys
    /// derive from usage telemetry's PGHACKER_USAGE_ID_KEY. On, it starts a
    /// task, in the Tokio runtime that usage telemetry also needs, that
    /// forgets expired asks every minute.
    pub fn from_env(env: &Env, usage: &Usage) -> Result<Self> {
        let rate: f64 = env.parse_or("PGHACKER_FEEDBACK_RATE", 0.0)?;
        ensure!(
            (0.0..=1.0).contains(&rate),
            "PGHACKER_FEEDBACK_RATE must be from 0 to 1, such as 0.1"
        );
        let comments = env.parse_or("PGHACKER_FEEDBACK_COMMENTS", false)?;
        let defaults = Settings::default();
        let question_gap: u64 = env.parse_or(
            "PGHACKER_FEEDBACK_QUESTION_GAP",
            defaults.question_gap.as_secs(),
        )?;
        let session_gap: u64 = env.parse_or(
            "PGHACKER_FEEDBACK_SESSION_GAP",
            defaults.session_gap.as_secs(),
        )?;
        ensure!(
            question_gap >= 1,
            "PGHACKER_FEEDBACK_QUESTION_GAP must be at least 1 (seconds)"
        );
        ensure!(
            session_gap > question_gap,
            "PGHACKER_FEEDBACK_SESSION_GAP must be more than PGHACKER_FEEDBACK_QUESTION_GAP"
        );
        let min_questions: u32 =
            env.parse_or("PGHACKER_FEEDBACK_MIN_QUESTIONS", defaults.min_questions)?;
        ensure!(
            min_questions <= 100,
            "PGHACKER_FEEDBACK_MIN_QUESTIONS must be from 0 to 100"
        );
        let clients = match env.get("PGHACKER_FEEDBACK_CLIENTS") {
            None => defaults.clients,
            Some(list) => clients(list)?,
        };
        let debug = env.parse_or("PGHACKER_FEEDBACK_DEBUG", false)?;
        let settings = Settings {
            rate,
            comments,
            question_gap: Duration::from_secs(question_gap),
            session_gap: Duration::from_secs(session_gap),
            min_questions,
            clients,
            debug,
        };
        if rate == 0.0 {
            return Ok(Self::off());
        }
        if !usage.is_on() {
            tracing::warn!(
                "PGHACKER_FEEDBACK_RATE is set, but usage telemetry is off: so is feedback"
            );
            return Ok(Self::off());
        }
        let id_key = env.require("PGHACKER_USAGE_ID_KEY")?.trim();
        let feedback = Self::new(settings, id_key.as_bytes())?;
        if let Some(inner) = &feedback.inner {
            tokio::spawn(forget_in_background(Arc::downgrade(inner)));
        }
        Ok(feedback)
    }

    /// Feedback with `settings`, its keys derived from `id_key`.
    pub fn new(settings: Settings, id_key: &[u8]) -> Result<Self> {
        ensure!(
            (0.0..=1.0).contains(&settings.rate),
            "the feedback rate must be from 0 to 1"
        );
        ensure!(
            !settings.question_gap.is_zero() && settings.session_gap > settings.question_gap,
            "the session gap must be longer than the question gap, which is above 0"
        );
        ensure!(
            settings.min_questions <= 100,
            "the questions before an ask must be from 0 to 100"
        );
        ensure!(
            settings.clients.iter().all(|name| !name.trim().is_empty()),
            "a feedback client's name is empty"
        );
        if settings.rate == 0.0 {
            return Ok(Self::off());
        }
        let tokens = Tokens::new(
            &derive(id_key, b"pghacker-mcp/feedback/state/v1"),
            token::LIFETIME,
        )?;
        tracing::info!(
            rate = settings.rate,
            comments = settings.comments,
            question_gap_s = settings.question_gap.as_secs_f64(),
            session_gap_s = settings.session_gap.as_secs_f64(),
            min_questions = settings.min_questions,
            clients = ?settings.clients,
            debug = settings.debug,
            "feedback is on"
        );
        if settings.debug {
            tracing::warn!(
                "feedback is in debug mode: every question after a client's first is asked; \
                 for trying the form, never for users"
            );
        }
        let clients = settings
            .clients
            .iter()
            .map(|name| name.trim().to_lowercase())
            .collect();
        Ok(Self {
            inner: Some(Arc::new(Inner {
                sampler: Sampler::new(derive(id_key, b"pghacker-mcp/feedback/draw/v1")),
                tokens,
                state: Mutex::new(State::default()),
                clients,
                settings,
            })),
        })
    }

    pub fn is_on(&self) -> bool {
        self.inner.is_some()
    }

    /// A call of `tool` by `caller` at `now` that carries no answer to a
    /// form: run it, or, when it starts a question that can be asked about
    /// the one before, hold it for the form.
    pub fn arrive(&self, caller: &Caller<'_>, tool: &str, now: SystemTime) -> Arrival {
        let Some(inner) = &self.inner else {
            return Arrival::Run(Call::none());
        };
        let Some(key) = inner.key(caller) else {
            return Arrival::Run(Call::none());
        };
        let now_ms = millis(now);
        let secs = now_ms / 1000;
        let mut state = inner.lock();
        let arrival =
            state
                .questions
                .arrive(&key, now_ms, inner.question_gap(), inner.session_gap());
        let rated = match arrival {
            questions::Arrival::Untracked => return Arrival::Run(Call::none()),
            questions::Arrival::Same { number, gap } => {
                if inner.settings.debug {
                    tracing::info!(
                        tool,
                        question = number,
                        gap_s = gap as f64 / 1000.0,
                        "feedback: the same question, not asked"
                    );
                }
                None
            }
            questions::Arrival::New { number, previous } => {
                match inner.decide(&mut state, &key.user, number, previous, secs) {
                    Ok(rated) => Some(rated),
                    Err(why) => {
                        let gap_s = previous.map(|(_, gap)| gap as f64 / 1000.0);
                        if inner.settings.debug {
                            tracing::info!(
                                tool,
                                question = number,
                                gap_s,
                                why,
                                "feedback: a new question, not asked"
                            );
                        } else {
                            tracing::debug!(
                                tool,
                                question = number,
                                gap_s,
                                why,
                                "feedback: a new question, not asked"
                            );
                        }
                        None
                    }
                }
            }
        };
        let Some((question, gap)) = rated else {
            drop(state);
            return Arrival::Run(Call {
                track: Some((inner.clone(), key)),
            });
        };
        state.questions.hold(&key, now_ms);
        let (request_state, id) = inner.tokens.issue(&key.app, tool);
        state.keep_ask(id, &key.user, caller.address, secs);
        drop(state);
        Arrival::Ask(Ask {
            form: form::form(inner.settings.comments, request_state),
            ask_id: id.simple().to_string(),
            rated: Rated {
                number: question.number,
                calls: question.calls,
                failed: question.failed,
                start: time(question.start),
                end: time(question.end),
                gap: Duration::from_millis(gap),
            },
        })
    }

    /// The retry of a call of `tool` by `caller` at `now`, carrying
    /// `responses` and `state`. It's never held, and belongs to the question
    /// the held call started. When it answers this server's form, from a
    /// client that can be asked, the answer is recorded once, if the state
    /// is the caller's app's for this tool and current, within the limits,
    /// whichever address it comes from.
    pub fn resume(
        &self,
        caller: &Caller<'_>,
        tool: &str,
        state: Option<&str>,
        responses: Option<&InputResponses>,
        now: SystemTime,
    ) -> Resumed {
        let Some(inner) = &self.inner else {
            return Resumed {
                call: Call::none(),
                outcome: None,
                rejection: None,
            };
        };
        let now_ms = millis(now);
        let secs = now_ms / 1000;
        let key = inner.key(caller);
        let mut guard = inner.lock();
        let tracked = key
            .as_ref()
            .is_some_and(|key| guard.questions.resume(key, now_ms, inner.session_gap()));
        let ours = state.is_some_and(Tokens::is_ours)
            || responses.is_some_and(|responses| responses.contains_key(form::KEY));
        let mut outcome = None;
        let mut rejection = None;
        // Only a client that could have been asked has an answer to record.
        if let (Some(key), true) = (&key, ours) {
            let user = key.user.as_str();
            match state.and_then(|state| inner.tokens.open(state, &key.app, tool)) {
                None => rejection = guard.reject(user, secs).then_some("invalid"),
                Some(id) if guard.answered(&id, secs) => {
                    rejection = guard.reject(user, secs).then_some("used");
                }
                Some(_) if !guard.within_limits(user, secs, inner.settings.debug) => {
                    rejection = guard.reject(user, secs).then_some("limit");
                }
                Some(id) => {
                    let answer = form::answer(
                        responses.and_then(|responses| responses.get(form::KEY)),
                        inner.settings.comments,
                    );
                    let asked = guard.take_ask(&id, secs);
                    let answered_from = answered_from(asked.as_ref(), user, caller.address);
                    guard.record(
                        user,
                        asked.as_ref().map(|asked| asked.user.as_str()),
                        id,
                        secs,
                        answer.outcome == "declined",
                    );
                    outcome = Some(Outcome {
                        ask_id: id.simple().to_string(),
                        outcome: answer.outcome,
                        rating: answer.rating,
                        comment: answer.comment,
                        answered_from,
                    });
                }
            }
        }
        drop(guard);
        let call = match key {
            Some(key) if tracked => Call {
                track: Some((inner.clone(), key)),
            },
            _ => Call::none(),
        };
        Resumed {
            call,
            outcome,
            rejection,
        }
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("feedback's lock")
    }

    /// Whether the start of question `number`, after `previous` and the
    /// pause since it (milliseconds), is asked about it at `secs`: the
    /// question to rate and the pause, or why not. In debug mode, every one
    /// after the client's first is.
    fn decide(
        &self,
        state: &mut State,
        user: &str,
        number: u32,
        previous: Option<(Question, u64)>,
        secs: u64,
    ) -> Result<(Question, u64), &'static str> {
        let Some((question, gap)) = previous else {
            return Err("the client's first question");
        };
        if self.settings.debug {
            return Ok((question, gap));
        }
        if gap >= self.session_gap() {
            return Err("the session was idle");
        }
        if number <= self.settings.min_questions {
            return Err("the warm-up");
        }
        if !question.succeeded() {
            return Err("no call of the question before succeeded");
        }
        if !self.sampler.armed(user, secs / 3600, self.settings.rate) {
            return Err("the hour isn't drawn");
        }
        state.ask(user, secs)?;
        Ok((question, gap))
    }

    /// The client `caller` is, if it can be followed and asked: an address,
    /// an app whose name contains one on the list, ignoring case, a form it
    /// can show, and feedback not turned off. Apps are told apart by their
    /// whole names.
    fn key(&self, caller: &Caller<'_>) -> Option<Key> {
        let user = caller.user?;
        if caller.opted_out || !caller.elicits {
            return None;
        }
        let app = caller.app.as_deref()?.trim().to_lowercase();
        self.clients
            .iter()
            .any(|name| app.contains(name.as_str()))
            .then(|| Key {
                user: user.to_owned(),
                app,
            })
    }

    fn question_gap(&self) -> u64 {
        self.settings.question_gap.as_millis() as u64
    }

    fn session_gap(&self) -> u64 {
        self.settings.session_gap.as_millis() as u64
    }
}

/// The apps in PGHACKER_FEEDBACK_CLIENTS: names separated by commas.
fn clients(list: &str) -> Result<Vec<String>> {
    list.split(',')
        .map(|name| {
            let name = name.trim();
            ensure!(
                !name.is_empty(),
                "PGHACKER_FEEDBACK_CLIENTS has an empty name: {list:?}"
            );
            Ok(name.to_owned())
        })
        .collect()
}

/// A key for `label`'s use, derived from usage telemetry's key.
fn derive(id_key: &[u8], label: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(id_key).expect("HMAC takes keys of any length");
    mac.update(label);
    let mut key = [0u8; 32];
    key.copy_from_slice(&mac.finalize().into_bytes());
    key
}

fn millis(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

fn time(millis: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(millis)
}

/// Forgets the asks whose state has expired every [`SWEEP`], so that an ask
/// never answered isn't kept past its expiry, until feedback is dropped.
async fn forget_in_background(inner: Weak<Inner>) {
    loop {
        tokio::time::sleep(SWEEP).await;
        let Some(inner) = inner.upgrade() else {
            return;
        };
        inner.lock().forget_asks(millis(SystemTime::now()) / 1000);
    }
}

/// The part of `address` an ask keeps: its /24, or for IPv6 its /48, all
/// that telling how near an answer's address is takes.
fn network(address: IpAddr) -> IpAddr {
    match address.to_canonical() {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            IpAddr::V4(Ipv4Addr::new(a, b, c, 0))
        }
        IpAddr::V6(ip) => {
            let [a, b, c, ..] = ip.segments();
            IpAddr::V6(Ipv6Addr::new(a, b, c, 0, 0, 0, 0, 0))
        }
    }
}

/// Where an answer from `user` at `address` came from, against the ask
/// `asked`: see [`Outcome::answered_from`].
fn answered_from(asked: Option<&Asked>, user: &str, address: Option<IpAddr>) -> &'static str {
    let Some(asked) = asked else {
        return "unknown";
    };
    if asked.user == user {
        return "same";
    }
    let (Some(a), Some(b)) = (asked.address, address) else {
        return "other";
    };
    match (a.to_canonical(), b.to_canonical()) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let (a, b) = (a.octets(), b.octets());
            if a[..3] == b[..3] {
                "same24"
            } else if a[..2] == b[..2] {
                "same16"
            } else {
                "other"
            }
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let (a, b) = (a.segments(), b.segments());
            if a[..3] == b[..3] {
                "same24"
            } else if a[..2] == b[..2] {
                "same16"
            } else {
                "other"
            }
        }
        _ => "other",
    }
}

/// What feedback keeps, in memory.
#[derive(Default)]
struct State {
    questions: Questions,
    addresses: HashMap<String, Address>,
    /// The asks not yet answered, until their state expires.
    asks: HashMap<Uuid, Asked>,
    /// The asks answered, until their state expires (unix seconds).
    answered: HashMap<Uuid, u64>,
    /// The current minute, and the outcomes recorded in it.
    minute: (u64, u32),
    /// The minute each address's last rejection was recorded in, and the
    /// current minute's rejections in all. Apart from `addresses`, so that
    /// rejections, which anyone can cause, never crowd out an address's
    /// asks and pause.
    rejected: HashMap<String, u64>,
    rejections: (u64, u32),
}

/// An ask: who was asked, to link with the address its answer comes from.
struct Asked {
    /// The address's user hash.
    user: String,
    /// The address's [`network`], kept in memory only.
    address: Option<IpAddr>,
    /// When its state expires (unix seconds).
    until: u64,
}

/// An address's asks and outcomes. Only an ask makes one.
#[derive(Default)]
struct Address {
    asked_hour: Option<u64>,
    paused_until: u64,
    /// The current hour, and the address's outcomes recorded in it.
    outcomes: (u64, u32),
}

impl Address {
    /// Whether any of this still matters at `secs`.
    fn current(&self, secs: u64) -> bool {
        let hour = secs / 3600;
        self.paused_until > secs || self.asked_hour == Some(hour) || self.outcomes.0 == hour
    }
}

impl State {
    /// `user`'s entry, made if there's room. When every entry still matters,
    /// there's none: such an address isn't asked, and no entry's pause is
    /// lost.
    fn address(&mut self, user: &str, secs: u64) -> Option<&mut Address> {
        if self.addresses.len() >= MAX_ADDRESSES && !self.addresses.contains_key(user) {
            self.addresses.retain(|_, address| address.current(secs));
            if self.addresses.len() >= MAX_ADDRESSES {
                return None;
            }
        }
        Some(self.addresses.entry(user.to_owned()).or_default())
    }

    /// Whether `user` may be asked at `secs`, marking its hour asked if so,
    /// or why not.
    fn ask(&mut self, user: &str, secs: u64) -> Result<(), &'static str> {
        let hour = secs / 3600;
        let Some(address) = self.address(user, secs) else {
            return Err("no room to keep the address");
        };
        if address.paused_until > secs {
            return Err("paused after a decline");
        }
        if address.asked_hour == Some(hour) {
            return Err("already asked this hour");
        }
        address.asked_hour = Some(hour);
        Ok(())
    }

    fn answered(&self, id: &Uuid, secs: u64) -> bool {
        self.answered.get(id).is_some_and(|until| *until > secs)
    }

    /// Keeps the ask `id`, made at `secs` of `user` at `address`, until its
    /// state expires. When the table is full of current asks, it isn't kept:
    /// its answer still counts, as from an `unknown` address.
    fn keep_ask(&mut self, id: Uuid, user: &str, address: Option<IpAddr>, secs: u64) {
        if self.asks.len() >= MAX_ASKS {
            self.forget_asks(secs);
            if self.asks.len() >= MAX_ASKS {
                return;
            }
        }
        self.asks.insert(
            id,
            Asked {
                user: user.to_owned(),
                address: address.map(network),
                until: secs + token::LIFETIME.as_secs(),
            },
        );
    }

    /// The ask `id`, if it's kept and current at `secs`, forgotten now.
    fn take_ask(&mut self, id: &Uuid, secs: u64) -> Option<Asked> {
        self.asks.remove(id).filter(|asked| asked.until > secs)
    }

    /// Forgets the asks whose state has expired at `secs`.
    fn forget_asks(&mut self, secs: u64) {
        self.asks.retain(|_, asked| asked.until > secs);
    }

    /// Whether an outcome of `user`'s at `secs` is within the limits: per
    /// minute in all, and, unless in debug mode, per address an hour.
    fn within_limits(&self, user: &str, secs: u64, debug: bool) -> bool {
        let hour = secs / 3600;
        let address = debug
            || self.addresses.get(user).is_none_or(|address| {
                address.outcomes.0 != hour || address.outcomes.1 < ADDRESS_OUTCOMES_PER_HOUR
            });
        let all = self.minute.0 != secs / 60 || self.minute.1 < OUTCOMES_PER_MINUTE;
        address && all
    }

    /// Records the outcome `user` gave at `secs` for the ask `id`, made of
    /// `asked`, if known. The hour's ask, and a decline's pause, count for
    /// the address that answered and, if another, the one asked.
    fn record(&mut self, user: &str, asked: Option<&str>, id: Uuid, secs: u64, declined: bool) {
        if self.answered.len() >= MAX_ANSWERED {
            self.answered.retain(|_, until| *until > secs);
            if self.answered.len() >= MAX_ANSWERED {
                self.answered.clear();
            }
        }
        self.answered.insert(id, secs + token::LIFETIME.as_secs());
        let minute = secs / 60;
        self.minute = (
            minute,
            if self.minute.0 == minute {
                self.minute.1 + 1
            } else {
                1
            },
        );
        let hour = secs / 3600;
        if let Some(address) = self.address(user, secs) {
            address.outcomes = (
                hour,
                if address.outcomes.0 == hour {
                    address.outcomes.1 + 1
                } else {
                    1
                },
            );
        }
        let other = asked.filter(|asked| *asked != user);
        for linked in std::iter::once(user).chain(other) {
            if let Some(address) = self.address(linked, secs) {
                address.asked_hour = Some(hour);
                if declined {
                    address.paused_until = secs + PAUSE;
                }
            }
        }
    }

    /// Whether to record a rejection of `user`'s at `secs`: once a minute
    /// per address, and [`REJECTIONS_PER_MINUTE`] in all.
    fn reject(&mut self, user: &str, secs: u64) -> bool {
        let minute = secs / 60;
        if self.rejections.0 == minute && self.rejections.1 >= REJECTIONS_PER_MINUTE {
            return false;
        }
        if self.rejected.get(user) == Some(&minute) {
            return false;
        }
        if self.rejected.len() >= MAX_ADDRESSES && !self.rejected.contains_key(user) {
            self.rejected.retain(|_, last| *last == minute);
        }
        self.rejected.insert(user.to_owned(), minute);
        self.rejections = (
            minute,
            if self.rejections.0 == minute {
                self.rejections.1 + 1
            } else {
                1
            },
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use serde_json::{Value, json};

    use super::*;
    use crate::usage::Settings as UsageSettings;

    thread_local! {
        /// When a call dropped on this thread ends, on the tests' clock
        /// ([`at`]), rather than the real time: the tests' times are fixed.
        pub(super) static DROPPED_AT: Cell<Option<SystemTime>> = const { Cell::new(None) };
    }

    const ID_KEY: &[u8] = b"test-usage-id-key-0123456789abcdef";
    const USER: &str = "57d404ffeaedb6e25cfdb7e423e6582d";
    const OTHER: &str = "ed2b6a28eaba55c9854936ca33436922";
    /// Questions 4 minutes apart: a new question each time, in an active
    /// session.
    const STEP: u64 = 240;

    fn feedback(settings: Settings) -> Feedback {
        Feedback::new(settings, ID_KEY).unwrap()
    }

    fn always() -> Feedback {
        feedback(Settings {
            rate: 1.0,
            ..Settings::default()
        })
    }

    fn caller(user: &str) -> Caller<'_> {
        Caller {
            user: Some(user),
            address: None,
            app: Some("copilot-cli".into()),
            elicits: true,
            opted_out: false,
        }
    }

    /// 2026-10-20T09:00:00Z, the start of an hour, and `secs` later.
    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_792_486_800 + secs)
    }

    /// A call of `search` at `t` that isn't asked, and ends `ok` a second
    /// later.
    fn run(feedback: &Feedback, caller: &Caller<'_>, t: u64, ok: bool) {
        match feedback.arrive(caller, "search", at(t)) {
            Arrival::Run(call) => call.end(ok, at(t + 1)),
            Arrival::Ask(_) => panic!("asked at {t}"),
        }
    }

    /// `n` questions of one call each, `STEP` apart from `t`, none asked;
    /// when the next would come.
    fn questions(feedback: &Feedback, caller: &Caller<'_>, t: u64, n: u32) -> u64 {
        let mut t = t;
        for _ in 0..n {
            run(feedback, caller, t, true);
            t += STEP;
        }
        t
    }

    /// The ask of a call of `tool` at `t`.
    fn ask(feedback: &Feedback, caller: &Caller<'_>, tool: &str, t: u64) -> Ask {
        match feedback.arrive(caller, tool, at(t)) {
            Arrival::Ask(ask) => ask,
            Arrival::Run(_) => panic!("not asked at {t}"),
        }
    }

    fn state_of(ask: &Ask) -> String {
        serde_json::to_value(&ask.form).unwrap()["requestState"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn responses(answer: Value) -> InputResponses {
        [(form::KEY.to_owned(), answer)].into_iter().collect()
    }

    fn accept(rating: &str) -> InputResponses {
        responses(json!({ "action": "accept", "content": { "rating": rating } }))
    }

    /// The retry of a call of `tool` at `t`, which ends a second later: what
    /// it recorded, or why not.
    fn resume(
        feedback: &Feedback,
        caller: &Caller<'_>,
        tool: &str,
        state: Option<&str>,
        responses: Option<&InputResponses>,
        t: u64,
    ) -> (Option<Outcome>, Option<&'static str>) {
        let resumed = feedback.resume(caller, tool, state, responses, at(t));
        resumed.call.end(true, at(t + 1));
        (resumed.outcome, resumed.rejection)
    }

    #[test]
    fn the_environment_checks_its_settings() {
        let off = Usage::off();
        let env = |pairs: &[(&str, &str)]| {
            let mut all = vec![
                ("PGHACKER_FEEDBACK_RATE", "0.1"),
                (
                    "PGHACKER_USAGE_ID_KEY",
                    "test-usage-id-key-0123456789abcdef",
                ),
            ];
            all.extend_from_slice(pairs);
            Env::from_pairs(all)
        };
        // Without usage telemetry, or unset, it's off.
        assert!(!Feedback::from_env(&env(&[]), &off).unwrap().is_on());
        assert!(!Feedback::from_env(&Env::default(), &off).unwrap().is_on());
        for (key, bad) in [
            ("PGHACKER_FEEDBACK_RATE", "1.5"),
            ("PGHACKER_FEEDBACK_RATE", "-0.1"),
            ("PGHACKER_FEEDBACK_RATE", "NaN"),
            ("PGHACKER_FEEDBACK_RATE", "10%"),
            ("PGHACKER_FEEDBACK_COMMENTS", "maybe"),
            ("PGHACKER_FEEDBACK_QUESTION_GAP", "0"),
            ("PGHACKER_FEEDBACK_QUESTION_GAP", "3m"),
            ("PGHACKER_FEEDBACK_SESSION_GAP", "180"),
            ("PGHACKER_FEEDBACK_SESSION_GAP", "-1"),
            ("PGHACKER_FEEDBACK_MIN_QUESTIONS", "101"),
            ("PGHACKER_FEEDBACK_MIN_QUESTIONS", "five"),
            ("PGHACKER_FEEDBACK_CLIENTS", "copilot-cli,,codex-mcp-client"),
            ("PGHACKER_FEEDBACK_DEBUG", "always"),
        ] {
            let error = Feedback::from_env(&env(&[(key, bad)]), &off).err();
            assert!(
                error.is_some_and(|error| format!("{error:#}").contains(key)),
                "{key}={bad}"
            );
        }
    }

    #[tokio::test]
    async fn with_usage_telemetry_on_the_environment_sets_it() {
        let usage = Usage::new(UsageSettings {
            endpoint: url::Url::parse("http://127.0.0.1:9/").unwrap(),
            instrumentation_key: "11111111-2222-4333-8444-555555555555".into(),
            id_key: ID_KEY.to_vec(),
            role: "pghacker-mcp".into(),
            version: "0.1.0".into(),
            instance: None,
            details: true,
        })
        .unwrap();
        let env = |pairs: &[(&str, &str)]| {
            let mut all = vec![(
                "PGHACKER_USAGE_ID_KEY",
                "test-usage-id-key-0123456789abcdef",
            )];
            all.extend_from_slice(pairs);
            Env::from_pairs(all)
        };
        let settings = |pairs: &[(&str, &str)]| {
            let feedback = Feedback::from_env(&env(pairs), &usage).unwrap();
            let inner = feedback.inner.expect("on");
            (inner.settings.clone(), inner.clients.clone())
        };
        assert!(
            !Feedback::from_env(&env(&[("PGHACKER_FEEDBACK_RATE", "0")]), &usage)
                .unwrap()
                .is_on()
        );
        let (defaults, clients) = settings(&[("PGHACKER_FEEDBACK_RATE", "0.1")]);
        assert_eq!(
            defaults,
            Settings {
                rate: 0.1,
                ..Settings::default()
            }
        );
        assert_eq!(clients[2], "visual studio code");
        let (set, clients) = settings(&[
            ("PGHACKER_FEEDBACK_RATE", "1"),
            ("PGHACKER_FEEDBACK_COMMENTS", "true"),
            ("PGHACKER_FEEDBACK_QUESTION_GAP", "20"),
            ("PGHACKER_FEEDBACK_SESSION_GAP", "300"),
            ("PGHACKER_FEEDBACK_MIN_QUESTIONS", "1"),
            ("PGHACKER_FEEDBACK_CLIENTS", " copilot-cli , My App "),
            ("PGHACKER_FEEDBACK_DEBUG", "true"),
        ]);
        assert_eq!(
            set,
            Settings {
                rate: 1.0,
                comments: true,
                question_gap: Duration::from_secs(20),
                session_gap: Duration::from_secs(300),
                min_questions: 1,
                clients: vec!["copilot-cli".into(), "My App".into()],
                debug: true,
            }
        );
        assert_eq!(clients, ["copilot-cli", "my app"]);
        let (blank, _) = settings(&[
            ("PGHACKER_FEEDBACK_RATE", "1"),
            ("PGHACKER_FEEDBACK_CLIENTS", " "),
        ]);
        assert_eq!(blank.clients, Settings::default().clients);
        usage.shutdown().await;
    }

    #[test]
    fn the_fourth_question_is_asked_about_the_third() {
        let feedback = always();
        let me = caller(USER);
        // The first three questions, and the calls within them, are never
        // asked.
        let t = questions(&feedback, &me, 0, 2);
        run(&feedback, &me, t, true);
        run(&feedback, &me, t + 30, true);
        let t = t + STEP;
        // The fourth question's first call is held for the form.
        let ask = ask(&feedback, &me, "query", t);
        assert_eq!(
            ask.rated,
            Rated {
                number: 3,
                calls: 2,
                failed: 0,
                start: at(t - STEP),
                end: at(t - STEP + 31),
                gap: Duration::from_secs(STEP - 31),
            }
        );
        assert_eq!(ask.ask_id.len(), 32);
        let form = serde_json::to_value(&ask.form).unwrap();
        assert_eq!(form["resultType"], "input_required");
        assert_eq!(
            form["inputRequests"]["feedback"]["params"]["message"],
            text::PROMPT
        );
        let state = state_of(&ask);
        assert!(state.starts_with("fb3."), "{state}");
        // Its retry, with the answer, runs as part of the fourth question.
        let (outcome, rejection) = resume(
            &feedback,
            &me,
            "query",
            Some(&state),
            Some(&accept("up")),
            t + 20,
        );
        assert_eq!(rejection, None);
        assert_eq!(
            outcome,
            Some(Outcome {
                ask_id: ask.ask_id.clone(),
                outcome: "rated",
                rating: Some("up"),
                comment: None,
                answered_from: "same",
            })
        );
        run(&feedback, &me, t + 60, true);
        // Asked once this hour: the questions after it aren't.
        let mut t = t + STEP;
        while t < 3600 {
            run(&feedback, &me, t, true);
            t += STEP;
        }
        // The next hour, the first question in an active session is.
        let next = self::ask(&feedback, &me, "search", t);
        assert!(next.rated.number > 4);
    }

    #[test]
    fn only_a_question_in_an_active_session_is_asked() {
        let feedback = always();
        let me = caller(USER);
        let t = questions(&feedback, &me, 0, 3);
        // Back after the session gap: a new question, but the session
        // wasn't active.
        let back = t - STEP + 1 + 600;
        run(&feedback, &me, back, true);
        // The next question, 4 minutes on, is.
        let ask = ask(&feedback, &me, "search", back + STEP);
        assert_eq!(ask.rated.number, 4);
        // Just over the question gap is a new question too; just under it,
        // the same one.
        let feedback = always();
        let t = questions(&feedback, &me, 0, 3);
        run(&feedback, &me, t - STEP + 1 + 180, true);
        assert_eq!(
            self::ask(&feedback, &me, "search", t - STEP + 1 + 180 + 1 + 181)
                .rated
                .number,
            3
        );
    }

    #[test]
    fn a_question_whose_calls_all_failed_is_not_rated() {
        let feedback = always();
        let me = caller(USER);
        let t = questions(&feedback, &me, 0, 2);
        // Question 3's calls fail, or are dropped as when the client goes
        // away.
        run(&feedback, &me, t, false);
        let Arrival::Run(call) = feedback.arrive(&me, "search", at(t + 10)) else {
            panic!("not asked within a question");
        };
        DROPPED_AT.set(Some(at(t + 11)));
        drop(call);
        DROPPED_AT.set(None);
        // Question 4 would rate it: it isn't asked. Question 5 is, about 4.
        run(&feedback, &me, t + STEP, true);
        assert_eq!(ask(&feedback, &me, "search", t + 2 * STEP).rated.number, 4);
    }

    #[test]
    fn who_is_never_asked() {
        let never = |feedback: &Feedback, caller: &Caller<'_>| {
            let mut t = 0;
            for _ in 0..20 {
                match feedback.arrive(caller, "search", at(t)) {
                    Arrival::Run(call) => call.end(true, at(t + 1)),
                    Arrival::Ask(_) => panic!("asked {caller:?}"),
                }
                t += STEP;
            }
        };
        let feedback = always();
        let without = |change: fn(&mut Caller<'static>)| {
            let mut caller = caller(USER);
            change(&mut caller);
            caller
        };
        never(&feedback, &without(|c| c.user = None));
        never(&feedback, &without(|c| c.opted_out = true));
        never(&feedback, &without(|c| c.elicits = false));
        never(&feedback, &without(|c| c.app = None));
        never(&feedback, &without(|c| c.app = Some("mcp".into())));
        never(&feedback, &without(|c| c.app = Some("pilot-cli".into())));
        never(&feedback, &without(|c| c.app = Some("VS Code".into())));
        never(&Feedback::off(), &caller(USER));
        let none = feedback_with(|settings| settings.clients.clear());
        never(&none, &caller(USER));
        assert!(!feedback_with(|settings| settings.rate = 0.0).is_on());
        // The list matches any name that contains one on it, ignoring case.
        let mine = feedback_with(|settings| settings.clients = vec!["My App".into()]);
        let mut app = caller(USER);
        app.app = Some("the MY APP client".into());
        let t = questions(&mine, &app, 0, 3);
        ask(&mine, &app, "search", t);
        never(&mine, &caller(OTHER));
    }

    #[test]
    fn the_default_list_matches_the_interactive_clients_names() {
        let feedback = always();
        let inner = feedback.inner.as_ref().unwrap();
        for (name, asked) in [
            ("copilot-cli", true),
            ("GitHub Copilot", true),
            ("codex-mcp-client", true),
            ("Visual Studio Code", true),
            ("Visual Studio Code - Insiders", true),
            ("claude-code", true),
            ("claude-ai", true),
            ("Claude Desktop", true),
            ("cursor-vscode", true),
            ("mcp", false),
            ("python-mcp-client", false),
            ("VS Code", false),
            ("", false),
        ] {
            let mut caller = caller(USER);
            caller.app = Some(name.into());
            assert_eq!(inner.key(&caller).is_some(), asked, "{name:?}");
        }
        // Two apps that match the same name are told apart.
        let mut code = caller(USER);
        code.app = Some("claude-code".into());
        let mut app = caller(USER);
        app.app = Some("Claude-AI".into());
        let (code, app) = (inner.key(&code).unwrap(), inner.key(&app).unwrap());
        assert_ne!(code, app);
        assert_eq!(app.app, "claude-ai");
    }

    fn feedback_with(change: impl FnOnce(&mut Settings)) -> Feedback {
        let mut settings = Settings {
            rate: 1.0,
            ..Settings::default()
        };
        change(&mut settings);
        feedback(settings)
    }

    #[test]
    fn settings_out_of_range_are_refused() {
        for change in [
            (|s: &mut Settings| s.rate = 1.5) as fn(&mut Settings),
            |s| s.question_gap = Duration::ZERO,
            |s| s.session_gap = s.question_gap,
            |s| s.min_questions = 101,
            |s| s.clients = vec!["copilot-cli".into(), " ".into()],
        ] {
            let mut settings = Settings {
                rate: 1.0,
                ..Settings::default()
            };
            change(&mut settings);
            assert!(Feedback::new(settings, ID_KEY).is_err());
        }
    }

    #[test]
    fn the_answer_is_recorded_once_from_any_address_of_its_app() {
        let feedback = always();
        let me = caller(USER);
        let t = questions(&feedback, &me, 0, 3);
        let ask = ask(&feedback, &me, "search", t);
        let state = state_of(&ask);
        // Another app's, another tool's, or a forged state records nothing:
        // one rejection a minute per address.
        let mut codex = caller(USER);
        codex.app = Some("codex-mcp-client".into());
        assert_eq!(
            resume(
                &feedback,
                &codex,
                "search",
                Some(&state),
                Some(&accept("up")),
                t + 1
            ),
            (None, Some("invalid"))
        );
        assert_eq!(
            resume(
                &feedback,
                &me,
                "query",
                Some(&state),
                Some(&accept("up")),
                t + 2
            ),
            (None, None)
        );
        let them = caller(OTHER);
        assert_eq!(
            resume(
                &feedback,
                &them,
                "search",
                Some("fb3.AAAA"),
                Some(&accept("up")),
                t + 3
            ),
            (None, Some("invalid"))
        );
        assert_eq!(
            resume(&feedback, &me, "search", None, Some(&accept("up")), t + 4),
            (None, None)
        );
        // Another address of the same app records the answer, once.
        let (outcome, rejection) = resume(
            &feedback,
            &them,
            "search",
            Some(&state),
            Some(&accept("down")),
            t + 70,
        );
        assert_eq!(rejection, None);
        assert_eq!(
            outcome.map(|outcome| (outcome.outcome, outcome.rating, outcome.answered_from)),
            Some(("rated", Some("down"), "other"))
        );
        assert_eq!(
            resume(
                &feedback,
                &me,
                "search",
                Some(&state),
                Some(&accept("up")),
                t + 130
            ),
            (None, Some("used"))
        );
        // A retry of something else, as another server's form would be,
        // records nothing and isn't rejected.
        let other = [("login".to_owned(), json!({ "action": "accept" }))]
            .into_iter()
            .collect();
        assert_eq!(
            resume(&feedback, &me, "search", None, Some(&other), t + 140),
            (None, None)
        );
        // Opted out, or from a client that's never asked, a retry records
        // nothing either, not even a rejection.
        let mut off = caller(USER);
        off.opted_out = true;
        assert_eq!(
            resume(
                &feedback,
                &off,
                "search",
                Some(&state),
                Some(&accept("up")),
                t + 200
            ),
            (None, None)
        );
        let mut script = caller(OTHER);
        script.app = Some("mcp".into());
        assert_eq!(
            resume(
                &feedback,
                &script,
                "search",
                Some("fb3.AAAA"),
                Some(&accept("up")),
                t + 300
            ),
            (None, None)
        );
    }

    /// `user` at `address`, with Copilot CLI.
    fn client<'a>(user: &'a str, address: &str) -> Caller<'a> {
        Caller {
            address: Some(address.parse().unwrap()),
            ..caller(user)
        }
    }

    #[test]
    fn an_answer_says_how_near_its_address_is_to_the_asked_one() {
        let feedback = always();
        let pairs = [
            ("198.51.100.10", "198.51.100.10", "same"),
            ("198.51.100.10", "198.51.100.77", "same24"),
            ("198.51.100.10", "198.51.7.7", "same16"),
            ("198.51.100.10", "203.0.113.5", "other"),
            ("2001:db8:1:2::1", "2001:db8:1:3::1", "same24"),
            ("2001:db8:1:2::1", "2001:db8:9::1", "same16"),
            ("2001:db8:1:2::1", "2001:db9::1", "other"),
            ("198.51.100.10", "2001:db8::1", "other"),
            ("::ffff:198.51.100.10", "198.51.100.20", "same24"),
        ];
        for (n, (asked_at, answered_at, from)) in pairs.into_iter().enumerate() {
            // Each pair its own addresses' users, so that each is asked.
            let asked_user = format!("a{n:031x}");
            let answering_user = if from == "same" {
                asked_user.clone()
            } else {
                format!("b{n:031x}")
            };
            let asked = client(&asked_user, asked_at);
            let t = questions(&feedback, &asked, 0, 3);
            let held = ask(&feedback, &asked, "search", t);
            let answering = client(&answering_user, answered_at);
            let (outcome, rejection) = resume(
                &feedback,
                &answering,
                "search",
                Some(&state_of(&held)),
                Some(&accept("up")),
                t + 9,
            );
            assert_eq!(rejection, None, "{asked_at} -> {answered_at}");
            assert_eq!(
                outcome.map(|outcome| outcome.answered_from),
                Some(from),
                "{asked_at} -> {answered_at}"
            );
        }
        // An ask the server doesn't know, as after a restart, still counts.
        let restarted = always();
        let asked = client(USER, "198.51.100.10");
        let t = questions(&feedback, &asked, 0, 3);
        let held = ask(&feedback, &asked, "search", t);
        let (outcome, rejection) = resume(
            &restarted,
            &client(OTHER, "198.51.100.11"),
            "search",
            Some(&state_of(&held)),
            Some(&accept("up")),
            t + 9,
        );
        assert_eq!(rejection, None);
        assert_eq!(
            outcome.map(|outcome| outcome.answered_from),
            Some("unknown")
        );
    }

    #[test]
    fn an_answer_from_another_address_counts_the_hours_ask_and_a_decline_for_both() {
        // The address that answered isn't asked in that hour either.
        let mut state = State::default();
        let id = Uuid::new_v4();
        assert_eq!(state.ask(USER, 0), Ok(()));
        state.keep_ask(id, USER, None, 0);
        assert!(
            state
                .take_ask(&id, 10)
                .is_some_and(|asked| asked.user == USER)
        );
        assert!(state.take_ask(&id, 10).is_none());
        state.record(OTHER, Some(USER), id, 10, false);
        assert_eq!(state.ask(OTHER, 20), Err("already asked this hour"));
        assert_eq!(state.ask(USER, 20), Err("already asked this hour"));
        assert_eq!(state.ask(OTHER, 3600), Ok(()));
        // A decline from another address pauses both for a day.
        let mut state = State::default();
        assert_eq!(state.ask(USER, 0), Ok(()));
        state.record(OTHER, Some(USER), Uuid::new_v4(), 10, true);
        for user in [USER, OTHER] {
            assert_eq!(state.ask(user, 3600), Err("paused after a decline"));
            assert_eq!(state.ask(user, 10 + PAUSE), Ok(()));
        }
        // From the address asked, or an ask the server doesn't know, only
        // the address that answered: its hour, as when it's later than the
        // ask's, and its pause.
        for asked in [Some(USER), None] {
            let mut state = State::default();
            state.record(USER, asked, Uuid::new_v4(), 3610, false);
            assert_eq!(state.ask(USER, 3620), Err("already asked this hour"));
            assert_eq!(state.ask(OTHER, 3620), Ok(()));
            let mut state = State::default();
            state.record(USER, asked, Uuid::new_v4(), 10, true);
            assert_eq!(state.ask(USER, 3600), Err("paused after a decline"));
            assert_eq!(state.ask(OTHER, 3600), Ok(()));
        }
        // An ask keeps only its address's network, and is forgotten past its
        // state's expiry, answered or not.
        let mut state = State::default();
        for (address, kept) in [
            ("198.51.100.10", "198.51.100.0"),
            ("::ffff:198.51.100.10", "198.51.100.0"),
            ("2001:db8:1:2:3:4:5:6", "2001:db8:1::"),
        ] {
            let id = Uuid::new_v4();
            state.keep_ask(id, USER, Some(address.parse().unwrap()), 0);
            assert_eq!(state.asks[&id].address, Some(kept.parse().unwrap()));
            assert!(state.take_ask(&id, token::LIFETIME.as_secs()).is_none());
        }
        let lifetime = token::LIFETIME.as_secs();
        let (early, late) = (Uuid::new_v4(), Uuid::new_v4());
        state.keep_ask(early, USER, None, 0);
        state.keep_ask(late, OTHER, None, 100);
        state.forget_asks(lifetime - 1);
        assert_eq!(state.asks.len(), 2);
        state.forget_asks(lifetime);
        assert!(!state.asks.contains_key(&early) && state.asks.contains_key(&late));
        state.forget_asks(lifetime + 100);
        assert!(state.asks.is_empty());

        // Through the flow: the address that answered, warmed up and armed,
        // isn't asked at its next question in that hour, as another such
        // address is.
        let feedback = always();
        let asked = client(USER, "198.51.100.10");
        let answering = client(OTHER, "198.51.100.77");
        let third = client("0123456789abcdef0123456789abcdef", "203.0.113.5");
        let t = questions(&feedback, &asked, 0, 3);
        questions(&feedback, &answering, 0, 3);
        questions(&feedback, &third, 0, 3);
        let held = ask(&feedback, &asked, "search", t);
        let (outcome, _) = resume(
            &feedback,
            &answering,
            "search",
            Some(&state_of(&held)),
            Some(&accept("up")),
            t + 9,
        );
        assert_eq!(outcome.map(|outcome| outcome.answered_from), Some("same24"));
        run(&feedback, &answering, t + 9 + STEP, true);
        ask(&feedback, &third, "search", t + 9 + STEP);
    }

    #[test]
    fn rejections_never_crowd_out_an_addresss_asks() {
        let mut state = State::default();
        // Rejections are limited apart from addresses, and make none.
        let users: Vec<String> = (0..100).map(|n| format!("{n:032x}")).collect();
        let recorded = users.iter().filter(|user| state.reject(user, 0)).count();
        assert_eq!(recorded, REJECTIONS_PER_MINUTE as usize);
        assert!(!state.reject(&users[0], 0));
        assert!(state.reject(&users[0], 60));
        assert!(state.addresses.is_empty());
        // Full of addresses that still matter, the table makes no room by
        // forgetting them: a newcomer isn't asked, and every pause stays.
        state.record(USER, None, Uuid::new_v4(), 0, true);
        for n in 1..MAX_ADDRESSES {
            assert_eq!(state.ask(&format!("{n:032x}"), 0), Ok(()));
        }
        assert_eq!(state.ask(OTHER, 0), Err("no room to keep the address"));
        assert_eq!(state.addresses.len(), MAX_ADDRESSES);
        assert!(state.addresses[USER].paused_until > 0);
        // An hour on, those only asked no longer matter, and room is made.
        assert_eq!(state.ask(OTHER, 3600), Ok(()));
        assert!(state.addresses[USER].paused_until > 3600);
        assert_eq!(state.addresses.len(), 2);
    }

    #[test]
    fn a_decline_pauses_asks_at_the_address_for_a_day_and_a_skip_does_not() {
        let feedback = always();
        let me = caller(USER);
        let t = questions(&feedback, &me, 0, 3);
        let ask = ask(&feedback, &me, "search", t);
        let declined = responses(json!({ "action": "decline" }));
        let (outcome, _) = resume(
            &feedback,
            &me,
            "search",
            Some(&state_of(&ask)),
            Some(&declined),
            t + 5,
        );
        assert_eq!(outcome.unwrap().outcome, "declined");
        // Not the next hours, nor another app at the address.
        let mut codex = caller(USER);
        codex.app = Some("codex-mcp-client".into());
        let codex_start = questions(&feedback, &codex, 4 * 3600, 3);
        run(&feedback, &codex, codex_start, true);
        let mut t = 2 * 3600;
        for _ in 0..5 {
            run(&feedback, &me, t, true);
            t += STEP;
        }
        // A day later, it is.
        let day = 86_400 + 3600;
        run(&feedback, &me, day, true);
        assert!(matches!(
            feedback.arrive(&me, "search", at(day + STEP)),
            Arrival::Ask(_)
        ));

        let feedback = always();
        let t = questions(&feedback, &me, 0, 3);
        let ask = self::ask(&feedback, &me, "search", t);
        let (outcome, _) = resume(
            &feedback,
            &me,
            "search",
            Some(&state_of(&ask)),
            Some(&accept("skip")),
            t + 5,
        );
        assert_eq!(
            outcome.map(|outcome| (outcome.outcome, outcome.rating)),
            Some(("skipped", None))
        );
        let mut t = 3600;
        run(&feedback, &me, t, true);
        t += STEP;
        self::ask(&feedback, &me, "search", t);
    }

    #[test]
    fn outcomes_are_limited_per_minute_in_all_and_per_address_an_hour() {
        let feedback = always();
        let users: Vec<String> = (0..61).map(|n| format!("{n:032x}")).collect();
        let states: Vec<String> = users
            .iter()
            .map(|user| {
                let caller = caller(user);
                let t = questions(&feedback, &caller, 0, 3);
                state_of(&ask(&feedback, &caller, "search", t))
            })
            .collect();
        let t = 2_000;
        for (n, (user, state)) in users.iter().zip(&states).enumerate() {
            let (outcome, rejection) = resume(
                &feedback,
                &caller(user),
                "search",
                Some(state),
                Some(&accept("up")),
                t,
            );
            if n < 60 {
                assert!(outcome.is_some(), "{n}");
            } else {
                assert_eq!((outcome, rejection), (None, Some("limit")));
            }
        }
        // An address's own limit, which its one ask an hour keeps far off.
        let mut state = State::default();
        for _ in 0..ADDRESS_OUTCOMES_PER_HOUR {
            assert!(state.within_limits(USER, 0, false));
            state.record(USER, None, Uuid::new_v4(), 0, false);
        }
        assert!(!state.within_limits(USER, 0, false));
        assert!(state.within_limits(USER, 3600, false));
        assert!(state.within_limits(OTHER, 0, false));
        // Debug mode has no limit per address.
        assert!(state.within_limits(USER, 0, true));
    }

    #[test]
    fn in_debug_mode_every_question_after_the_first_is_asked() {
        // Whatever the warm-up (3), the draw (almost never), the hour's ask,
        // a decline, the address's outcomes, an idle session, or a question
        // before whose calls all failed.
        let feedback = feedback_with(|settings| {
            settings.debug = true;
            settings.rate = 1e-9;
        });
        let me = caller(USER);
        run(&feedback, &me, 0, true);
        let mut t = STEP;
        for (n, answer) in ["decline", "up", "down", "skip"].into_iter().enumerate() {
            let asked = ask(&feedback, &me, "search", t);
            assert_eq!(asked.rated.number, n as u32 + 1);
            let response = match answer {
                "decline" => responses(json!({ "action": "decline" })),
                rating => accept(rating),
            };
            let (outcome, rejection) = resume(
                &feedback,
                &me,
                "search",
                Some(&state_of(&asked)),
                Some(&response),
                t + 5,
            );
            assert_eq!(rejection, None, "{answer}");
            assert!(outcome.is_some(), "{answer}");
            // A call within the question isn't.
            run(&feedback, &me, t + 30, true);
            t += STEP;
        }
        // After two hours away.
        let back = t + 7200;
        let after_a_pause = ask(&feedback, &me, "search", back);
        assert!(after_a_pause.rated.gap > Duration::from_secs(7000));
        let resumed = feedback.resume(
            &me,
            "search",
            Some(&state_of(&after_a_pause)),
            Some(&accept("up")),
            at(back + 5),
        );
        resumed.call.end(false, at(back + 6));
        // After that question, whose one call failed.
        let after_a_failure = ask(&feedback, &me, "search", back + 200);
        assert_eq!(
            (after_a_failure.rated.calls, after_a_failure.rated.failed),
            (1, 1)
        );
        // A client's first question, or a client that can't be asked, still
        // isn't.
        let mut script = caller(OTHER);
        script.app = Some("mcp".into());
        for client in [caller(OTHER), script] {
            run(&feedback, &client, 0, true);
            if client.app.as_deref() == Some("mcp") {
                run(&feedback, &client, STEP, true);
            }
        }
    }

    #[test]
    fn with_comments_the_answer_keeps_one() {
        let feedback = feedback_with(|settings| settings.comments = true);
        let me = caller(USER);
        let t = questions(&feedback, &me, 0, 3);
        let ask = ask(&feedback, &me, "search", t);
        let answer = responses(json!({ "action": "accept", "content": {
            "rating": "down", "remarks": "cited the wrong thread" } }));
        let (outcome, _) = resume(
            &feedback,
            &me,
            "search",
            Some(&state_of(&ask)),
            Some(&answer),
            t + 5,
        );
        assert_eq!(
            outcome.unwrap().comment.as_deref(),
            Some("cited the wrong thread")
        );
    }
}
