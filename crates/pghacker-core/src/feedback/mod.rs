// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! User feedback: now and then, whether an answer helped, asked through the
//! client's own prompt (MCP elicitation, carried by the 2026-07-28
//! protocol's multi round-trip requests).
//!
//! In an hour that the draw arms ([`sampler`]), the first successful tool
//! result for a client that declares form elicitation gets a note with a
//! signed `feedback_id` ([`token`]). Once the model has written its answer,
//! it calls `user_feedback` with the id ([`tool`]). The first call answers
//! `input_required` with a one-click form; the client asks the user and
//! retries the call with their choice, which the server records in usage
//! telemetry. Nothing is kept between the two requests. What is kept, in
//! memory, keeps the asks rare: per client, the hour last asked, a pause
//! after a decline and recent outcomes, and the ids already answered. A
//! restart forgets it; the draw doesn't change.
//!
//! Off unless usage telemetry is on and PGHACKER_FEEDBACK_RATE is above 0.
//! A request with no client identity (the internal listener) or with
//! `feedback=off` in its URL is never asked.

mod sampler;
mod text;
mod token;
mod tool;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, ensure};
use hmac::{Hmac, KeyInit, Mac};
use rmcp::ErrorData;
use rmcp::model::{
    Annotations, ContentBlock, InputRequiredResult, InputResponses, JsonObject, Role, TextContent,
    Tool,
};
use serde_json::Value;
use sha2::Sha256;
use uuid::Uuid;

use crate::config::Env;
use crate::usage::Usage;
use sampler::Sampler;
use token::Tokens;

pub use text::INSTRUCTIONS;
pub use tool::TOOL;

/// No ask for this long after a client declines.
const PAUSE: u64 = 24 * 3600;
/// Outcomes recorded at most per client an hour, and per minute in all.
const CLIENT_OUTCOMES_PER_HOUR: u32 = 3;
const OUTCOMES_PER_MINUTE: u32 = 60;
/// Clients and answered ids kept at most; beyond, the stale are forgotten.
const MAX_CLIENTS: usize = 100_000;
const MAX_ANSWERED: usize = 100_000;

/// What the environment sets.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    /// The chance, from 0 to 1, that a client's active hour has an ask.
    pub rate: f64,
    /// Whether the form has an optional comment field.
    pub comments: bool,
}

/// Feedback's handle: cheap to clone, and off by default.
#[derive(Clone, Default)]
pub struct Feedback {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    settings: Settings,
    sampler: Sampler,
    tokens: Tokens,
    state: Mutex<State>,
}

/// What a request tells feedback about its client.
#[derive(Clone, Copy, Debug, Default)]
pub struct Caller<'a> {
    /// The usage identity's user, a keyed hash of the client's address.
    /// Without one, as on the internal listener, a client is never asked.
    pub user: Option<&'a str>,
    /// Whether the request declares form elicitation (MCP 2026-07-28).
    pub elicits: bool,
    /// Whether the request's URL has `feedback=off`.
    pub opted_out: bool,
}

/// The note for a sampled result.
#[derive(Debug)]
pub struct Offer {
    /// A text block for the model, after the result's content.
    pub note: ContentBlock,
    /// The id's random value, which names the offer and its outcome in
    /// usage events.
    pub feedback_id: String,
}

/// `user_feedback`'s answer.
#[derive(Debug)]
pub enum Reply {
    /// The form: the client asks the user and retries the call.
    Ask(InputRequiredResult),
    /// A result, with the user's choice to record, if any.
    Done {
        text: &'static str,
        outcome: Option<Outcome>,
    },
    /// A tool error, with the rejection's reason to record, if any: at most
    /// once a minute per client.
    Refused {
        text: &'static str,
        rejection: Option<&'static str>,
    },
}

impl Reply {
    fn done(text: &'static str) -> Self {
        Reply::Done {
            text,
            outcome: None,
        }
    }
}

/// The user's choice, to record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// The id's random value, as its offer named it.
    pub feedback_id: String,
    /// `rated`, `declined` or `dismissed`.
    pub outcome: &'static str,
    /// `up` or `down`, when rated.
    pub rating: Option<&'static str>,
    /// With comments on, what the user wrote.
    pub comment: Option<String>,
}

impl Feedback {
    pub fn off() -> Self {
        Self::default()
    }

    /// Feedback as the environment configures it: PGHACKER_FEEDBACK_RATE,
    /// the chance from 0 (unset: off) to 1 that a client's active hour has
    /// an ask, and PGHACKER_FEEDBACK_COMMENTS. It's on only with usage
    /// telemetry, where it records, and its keys derive from usage
    /// telemetry's PGHACKER_USAGE_ID_KEY.
    pub fn from_env(env: &Env, usage: &Usage) -> Result<Self> {
        let rate: f64 = env.parse_or("PGHACKER_FEEDBACK_RATE", 0.0)?;
        ensure!(
            (0.0..=1.0).contains(&rate),
            "PGHACKER_FEEDBACK_RATE must be from 0 to 1, such as 0.1"
        );
        let comments = env.parse_or("PGHACKER_FEEDBACK_COMMENTS", false)?;
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
        Self::new(Settings { rate, comments }, id_key.as_bytes())
    }

    /// Feedback with `settings`, its keys derived from `id_key`.
    pub fn new(settings: Settings, id_key: &[u8]) -> Result<Self> {
        ensure!(
            (0.0..=1.0).contains(&settings.rate),
            "the feedback rate must be from 0 to 1"
        );
        if settings.rate == 0.0 {
            return Ok(Self::off());
        }
        let tokens = Tokens::new(
            &derive(id_key, b"pghacker-mcp/feedback/id/v1"),
            token::LIFETIME,
        )?;
        tracing::info!(
            rate = settings.rate,
            comments = settings.comments,
            "feedback is on"
        );
        Ok(Self {
            inner: Some(Arc::new(Inner {
                settings,
                sampler: Sampler::new(derive(id_key, b"pghacker-mcp/feedback/draw/v1")),
                tokens,
                state: Mutex::new(State::default()),
            })),
        })
    }

    pub fn is_on(&self) -> bool {
        self.inner.is_some()
    }

    /// `user_feedback` as tools/list describes it, when feedback is on.
    pub fn tool(&self) -> Option<Tool> {
        self.inner.as_ref().map(|_| tool::spec())
    }

    /// The note for a successful result of `caller`'s at `now`: when its
    /// hour is armed and it has had none this hour, nor declined lately.
    pub fn offer(&self, caller: &Caller<'_>, now: SystemTime) -> Option<Offer> {
        let inner = self.inner.as_ref()?;
        let user = caller.user?;
        if caller.opted_out || !caller.elicits {
            return None;
        }
        let secs = unix_secs(now);
        if !inner.sampler.armed(user, secs / 3600, inner.settings.rate) {
            return None;
        }
        if !inner.lock().ask(user, secs) {
            return None;
        }
        let (id, value) = inner.tokens.issue(user);
        let note = TextContent::new(text::note(&id)).with_annotations(
            Annotations::default()
                .with_audience(vec![Role::Assistant])
                .with_priority(0.2),
        );
        Some(Offer {
            note: ContentBlock::Text(note),
            feedback_id: value.simple().to_string(),
        })
    }

    /// `user_feedback`'s answer to `caller` at `now`, for `arguments` and,
    /// on the retry, the client's `responses` to the form.
    pub fn reply(
        &self,
        caller: &Caller<'_>,
        arguments: JsonObject,
        responses: Option<&InputResponses>,
        now: SystemTime,
    ) -> Result<Reply, ErrorData> {
        let Some(inner) = &self.inner else {
            return Err(ErrorData::invalid_params(
                format!("unknown tool {TOOL}"),
                None,
            ));
        };
        let args: tool::Args = crate::mcp::parse(TOOL, Value::Object(arguments))?;
        let Some(user) = caller.user else {
            return Ok(Reply::Refused {
                text: text::UNAVAILABLE,
                rejection: None,
            });
        };
        if caller.opted_out {
            return Ok(Reply::done(text::OPTED_OUT));
        }
        let secs = unix_secs(now);
        let Some(value) = inner.tokens.open(&args.feedback_id, user) else {
            let rejection = inner.lock().reject(user, secs).then_some("invalid");
            return Ok(Reply::Refused {
                text: text::INVALID,
                rejection,
            });
        };
        let mut state = inner.lock();
        if state.answered(&value, secs) || state.paused(user, secs) {
            return Ok(Reply::done(text::NOTHING_MORE));
        }
        let Some(responses) = responses else {
            return Ok(if caller.elicits {
                Reply::Ask(tool::form(inner.settings.comments))
            } else {
                Reply::done(text::CANNOT_ASK)
            });
        };
        if !state.within_limits(user, secs) {
            let rejection = state.reject(user, secs).then_some("limit");
            return Ok(Reply::Refused {
                text: text::TOO_MUCH,
                rejection,
            });
        }
        let answer = tool::answer(responses.get(tool::KEY), inner.settings.comments);
        state.record(user, value, secs, answer.outcome == "declined");
        Ok(Reply::Done {
            text: answer.text(),
            outcome: Some(Outcome {
                feedback_id: value.simple().to_string(),
                outcome: answer.outcome,
                rating: answer.rating,
                comment: answer.comment,
            }),
        })
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("feedback's lock")
    }
}

/// A key for `label`'s use, derived from usage telemetry's key.
fn derive(id_key: &[u8], label: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(id_key).expect("HMAC takes keys of any length");
    mac.update(label);
    let mut key = [0u8; 32];
    key.copy_from_slice(&mac.finalize().into_bytes());
    key
}

fn unix_secs(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// What feedback keeps, in memory.
#[derive(Default)]
struct State {
    clients: HashMap<String, Client>,
    /// The ids answered, until they expire (unix seconds).
    answered: HashMap<Uuid, u64>,
    /// The current minute, and the outcomes recorded in it.
    minute: (u64, u32),
}

#[derive(Default)]
struct Client {
    asked_hour: Option<u64>,
    paused_until: u64,
    /// The current hour, and the client's outcomes recorded in it.
    outcomes: (u64, u32),
    rejected_minute: Option<u64>,
}

impl Client {
    /// Whether any of this still matters at `secs`.
    fn current(&self, secs: u64) -> bool {
        let hour = secs / 3600;
        self.paused_until > secs
            || self.asked_hour == Some(hour)
            || self.outcomes.0 == hour
            || self.rejected_minute == Some(secs / 60)
    }
}

impl State {
    fn client(&mut self, user: &str, secs: u64) -> &mut Client {
        if self.clients.len() >= MAX_CLIENTS && !self.clients.contains_key(user) {
            self.clients.retain(|_, client| client.current(secs));
            if self.clients.len() >= MAX_CLIENTS {
                self.clients.clear();
            }
        }
        self.clients.entry(user.to_owned()).or_default()
    }

    /// Whether `user` may be asked at `secs`, marking its hour asked if so.
    fn ask(&mut self, user: &str, secs: u64) -> bool {
        let hour = secs / 3600;
        let client = self.client(user, secs);
        if client.paused_until > secs || client.asked_hour == Some(hour) {
            return false;
        }
        client.asked_hour = Some(hour);
        true
    }

    fn paused(&self, user: &str, secs: u64) -> bool {
        self.clients
            .get(user)
            .is_some_and(|client| client.paused_until > secs)
    }

    fn answered(&self, value: &Uuid, secs: u64) -> bool {
        self.answered.get(value).is_some_and(|until| *until > secs)
    }

    fn within_limits(&self, user: &str, secs: u64) -> bool {
        let hour = secs / 3600;
        let client = self.clients.get(user).is_none_or(|client| {
            client.outcomes.0 != hour || client.outcomes.1 < CLIENT_OUTCOMES_PER_HOUR
        });
        let all = self.minute.0 != secs / 60 || self.minute.1 < OUTCOMES_PER_MINUTE;
        client && all
    }

    /// Records `user`'s outcome for the id `value` at `secs`.
    fn record(&mut self, user: &str, value: Uuid, secs: u64, declined: bool) {
        if self.answered.len() >= MAX_ANSWERED {
            self.answered.retain(|_, until| *until > secs);
            if self.answered.len() >= MAX_ANSWERED {
                self.answered.clear();
            }
        }
        self.answered
            .insert(value, secs + token::LIFETIME.as_secs());
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
        let client = self.client(user, secs);
        client.outcomes = (
            hour,
            if client.outcomes.0 == hour {
                client.outcomes.1 + 1
            } else {
                1
            },
        );
        if declined {
            client.paused_until = secs + PAUSE;
        }
    }

    /// Whether to record a rejection of `user`'s at `secs`: once a minute.
    fn reject(&mut self, user: &str, secs: u64) -> bool {
        let minute = secs / 60;
        let client = self.client(user, secs);
        if client.rejected_minute == Some(minute) {
            return false;
        }
        client.rejected_minute = Some(minute);
        true
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::usage::Settings as UsageSettings;

    const ID_KEY: &[u8] = b"test-usage-id-key-0123456789abcdef";
    const USER: &str = "57d404ffeaedb6e25cfdb7e423e6582d";
    const OTHER: &str = "ed2b6a28eaba55c9854936ca33436922";

    fn always(comments: bool) -> Feedback {
        Feedback::new(
            Settings {
                rate: 1.0,
                comments,
            },
            ID_KEY,
        )
        .unwrap()
    }

    fn caller(user: &str) -> Caller<'_> {
        Caller {
            user: Some(user),
            elicits: true,
            opted_out: false,
        }
    }

    /// 2026-10-20T09:00:00Z, the start of an hour.
    fn at(seconds_later: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_792_486_800 + seconds_later)
    }

    fn id_of(offer: &Offer) -> String {
        let ContentBlock::Text(note) = &offer.note else {
            panic!("a text note");
        };
        let start = note.text.find("fb1.").unwrap();
        note.text[start..]
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned()
    }

    fn arguments(id: &str) -> JsonObject {
        json!({ "feedback_id": id }).as_object().unwrap().clone()
    }

    fn responses(answer: Value) -> InputResponses {
        [(tool::KEY.to_owned(), answer)].into_iter().collect()
    }

    fn accept(rating: &str) -> InputResponses {
        responses(json!({ "action": "accept", "content": { "rating": rating } }))
    }

    #[test]
    fn the_environment_turns_it_on_only_with_usage_telemetry() {
        let usage_env = |rate: &str| {
            Env::from_pairs([
                ("PGHACKER_FEEDBACK_RATE", rate),
                (
                    "PGHACKER_USAGE_ID_KEY",
                    "test-usage-id-key-0123456789abcdef",
                ),
            ])
        };
        let off = Usage::off();
        assert!(!Feedback::from_env(&usage_env("0.1"), &off).unwrap().is_on());
        assert!(!Feedback::from_env(&Env::default(), &off).unwrap().is_on());
        for bad in ["1.5", "-0.1", "NaN", "10%"] {
            let error = Feedback::from_env(&usage_env(bad), &off).err();
            assert!(
                error.is_some_and(|error| format!("{error:#}").contains("PGHACKER_FEEDBACK_RATE")),
                "{bad}"
            );
        }
        let comments = Env::from_pairs([("PGHACKER_FEEDBACK_COMMENTS", "maybe")]);
        assert!(Feedback::from_env(&comments, &off).is_err());
    }

    #[tokio::test]
    async fn with_usage_telemetry_on_the_rate_turns_it_on() {
        let usage = Usage::new(UsageSettings {
            endpoint: url::Url::parse("http://127.0.0.1:9/").unwrap(),
            instrumentation_key: "11111111-2222-4333-8444-555555555555".into(),
            id_key: ID_KEY.to_vec(),
            role: "pghacker-mcp".into(),
            version: "0.1.0".into(),
            instance: None,
        })
        .unwrap();
        let env = |rate: &str| {
            Env::from_pairs([
                ("PGHACKER_FEEDBACK_RATE", rate),
                (
                    "PGHACKER_USAGE_ID_KEY",
                    "test-usage-id-key-0123456789abcdef",
                ),
            ])
        };
        assert!(Feedback::from_env(&env("0.1"), &usage).unwrap().is_on());
        assert!(!Feedback::from_env(&env("0"), &usage).unwrap().is_on());
        usage.shutdown().await;
    }

    #[test]
    fn a_client_is_asked_once_an_hour_and_only_if_it_can_be() {
        let feedback = always(false);
        assert!(feedback.offer(&caller(USER), at(0)).is_some());
        assert!(feedback.offer(&caller(USER), at(60)).is_none());
        assert!(feedback.offer(&caller(USER), at(3599)).is_none());
        // The next hour, and another client, are asked.
        assert!(feedback.offer(&caller(USER), at(3600)).is_some());
        assert!(feedback.offer(&caller(OTHER), at(60)).is_some());
        // Never without an identity, without elicitation, or opted out.
        let mut cannot = caller("c");
        cannot.elicits = false;
        assert!(feedback.offer(&cannot, at(0)).is_none());
        let mut out = caller("d");
        out.opted_out = true;
        assert!(feedback.offer(&out, at(0)).is_none());
        let anonymous = Caller {
            user: None,
            elicits: true,
            opted_out: false,
        };
        assert!(feedback.offer(&anonymous, at(0)).is_none());
        // Off, never.
        assert!(Feedback::off().offer(&caller("e"), at(0)).is_none());
        assert!(Feedback::off().tool().is_none());
        // At 0.1, about one hour in ten.
        let sometimes = Feedback::new(
            Settings {
                rate: 0.1,
                comments: false,
            },
            ID_KEY,
        )
        .unwrap();
        let asked = (0..2_000u64)
            .filter(|hour| sometimes.offer(&caller(USER), at(hour * 3600)).is_some())
            .count();
        assert!((140..=260).contains(&asked), "{asked}");
    }

    #[test]
    fn the_first_call_asks_and_the_retry_records_the_choice_once() {
        let feedback = always(false);
        let offer = feedback.offer(&caller(USER), at(0)).unwrap();
        let id = id_of(&offer);

        let asked = feedback
            .reply(&caller(USER), arguments(&id), None, at(10))
            .unwrap();
        assert!(matches!(asked, Reply::Ask(_)), "{asked:?}");
        let recorded = feedback
            .reply(&caller(USER), arguments(&id), Some(&accept("up")), at(20))
            .unwrap();
        let Reply::Done { text, outcome } = recorded else {
            panic!("{recorded:?}");
        };
        assert_eq!(text, text::RECORDED);
        assert_eq!(
            outcome,
            Some(Outcome {
                feedback_id: offer.feedback_id.clone(),
                outcome: "rated",
                rating: Some("up"),
                comment: None,
            })
        );
        // Once: neither the form nor another outcome again.
        for responses in [None, Some(accept("down"))] {
            let again = feedback
                .reply(&caller(USER), arguments(&id), responses.as_ref(), at(30))
                .unwrap();
            assert!(
                matches!(
                    again,
                    Reply::Done {
                        text: text::NOTHING_MORE,
                        outcome: None
                    }
                ),
                "{again:?}"
            );
        }
    }

    #[test]
    fn a_decline_pauses_the_asks_for_a_day() {
        let feedback = always(false);
        let id = id_of(&feedback.offer(&caller(USER), at(0)).unwrap());
        let later = id_of(&feedback.offer(&caller(USER), at(3600)).unwrap());
        let declined = feedback
            .reply(
                &caller(USER),
                arguments(&id),
                Some(&responses(json!({ "action": "decline" }))),
                at(3700),
            )
            .unwrap();
        assert!(
            matches!(&declined, Reply::Done { text: text::NO_RATING, outcome: Some(o) } if o.outcome == "declined"),
            "{declined:?}"
        );
        // No ask for 24 hours, and an id issued earlier asks nothing.
        for hours in [2, 12, 24] {
            assert!(
                feedback.offer(&caller(USER), at(hours * 3600)).is_none(),
                "{hours}"
            );
        }
        let earlier = feedback
            .reply(&caller(USER), arguments(&later), None, at(3800))
            .unwrap();
        assert!(
            matches!(
                earlier,
                Reply::Done {
                    text: text::NOTHING_MORE,
                    ..
                }
            ),
            "{earlier:?}"
        );
        assert!(
            feedback
                .offer(&caller(USER), at(3700 + PAUSE + 3600))
                .is_some()
        );
        // A cancel doesn't pause.
        let other = id_of(&feedback.offer(&caller(OTHER), at(0)).unwrap());
        feedback
            .reply(
                &caller(OTHER),
                arguments(&other),
                Some(&responses(json!({ "action": "cancel" }))),
                at(10),
            )
            .unwrap();
        assert!(feedback.offer(&caller(OTHER), at(3600)).is_some());
    }

    #[test]
    fn ids_are_their_clients_and_rejections_are_recorded_once_a_minute() {
        let feedback = always(false);
        let id = id_of(&feedback.offer(&caller(USER), at(0)).unwrap());
        let refused = |user: &str, id: &str, secs: u64| {
            feedback
                .reply(&caller(user), arguments(id), None, at(secs))
                .unwrap()
        };
        assert!(matches!(
            refused(OTHER, &id, 10),
            Reply::Refused {
                text: text::INVALID,
                rejection: Some("invalid")
            }
        ));
        assert!(matches!(
            refused(OTHER, "fb1.forged", 20),
            Reply::Refused {
                text: text::INVALID,
                rejection: None
            }
        ));
        assert!(matches!(
            refused(OTHER, &id, 70),
            Reply::Refused {
                rejection: Some("invalid"),
                ..
            }
        ));
        // The id still works for its own client.
        assert!(matches!(refused(USER, &id, 80), Reply::Ask(_)));
    }

    #[test]
    fn what_cant_be_asked_records_nothing() {
        let feedback = always(false);
        let id = id_of(&feedback.offer(&caller(USER), at(0)).unwrap());
        let mut cannot = caller(USER);
        cannot.elicits = false;
        let reply = feedback
            .reply(&cannot, arguments(&id), None, at(10))
            .unwrap();
        assert!(
            matches!(
                reply,
                Reply::Done {
                    text: text::CANNOT_ASK,
                    outcome: None
                }
            ),
            "{reply:?}"
        );
        let mut out = caller(USER);
        out.opted_out = true;
        for responses in [None, Some(accept("up"))] {
            let reply = feedback
                .reply(&out, arguments(&id), responses.as_ref(), at(10))
                .unwrap();
            assert!(
                matches!(
                    reply,
                    Reply::Done {
                        text: text::OPTED_OUT,
                        outcome: None
                    }
                ),
                "{reply:?}"
            );
        }
        let anonymous = Caller::default();
        let reply = feedback
            .reply(&anonymous, arguments(&id), None, at(10))
            .unwrap();
        assert!(matches!(
            reply,
            Reply::Refused {
                text: text::UNAVAILABLE,
                rejection: None
            }
        ));
        // Arguments other than the id are the caller's mistake.
        for bad in [
            json!({}),
            json!({ "feedback_id": id, "rating": "up" }),
            json!({ "feedback_id": 7 }),
        ] {
            let error = feedback
                .reply(
                    &caller(USER),
                    bad.as_object().unwrap().clone(),
                    None,
                    at(10),
                )
                .unwrap_err();
            assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS, "{bad}");
        }
    }

    #[test]
    fn outcomes_are_limited_per_client_and_in_all() {
        let feedback = always(false);
        // One id an hour, answered in the sixth hour: three a client.
        let ids: Vec<String> = (0..5u64)
            .map(|hour| id_of(&feedback.offer(&caller(USER), at(hour * 3600)).unwrap()))
            .collect();
        let at_six = |n: u64| at(5 * 3600 + 100 + n);
        let mut kinds = Vec::new();
        for (n, id) in ids.iter().enumerate() {
            let reply = feedback
                .reply(
                    &caller(USER),
                    arguments(id),
                    Some(&accept("up")),
                    at_six(n as u64),
                )
                .unwrap();
            kinds.push(match reply {
                Reply::Done {
                    outcome: Some(_), ..
                } => "recorded",
                Reply::Refused {
                    text: text::TOO_MUCH,
                    rejection: Some("limit"),
                } => "limit",
                Reply::Refused {
                    text: text::TOO_MUCH,
                    rejection: None,
                } => "limit again",
                other => panic!("{other:?}"),
            });
        }
        assert_eq!(
            kinds,
            ["recorded", "recorded", "recorded", "limit", "limit again"]
        );

        // Sixty a minute in all.
        let feedback = always(false);
        let mut recorded = 0;
        for n in 0..70 {
            let user = format!("user-{n}");
            let id = id_of(&feedback.offer(&caller(&user), at(0)).unwrap());
            let reply = feedback
                .reply(
                    &caller(&user),
                    arguments(&id),
                    Some(&accept("down")),
                    at(30),
                )
                .unwrap();
            if matches!(
                reply,
                Reply::Done {
                    outcome: Some(_),
                    ..
                }
            ) {
                recorded += 1;
            }
        }
        assert_eq!(recorded, OUTCOMES_PER_MINUTE);
    }

    #[test]
    fn the_note_is_for_the_model() {
        let feedback = always(false);
        let offer = feedback.offer(&caller(USER), at(0)).unwrap();
        let note = serde_json::to_value(&offer.note).unwrap();
        assert_eq!(note["type"], "text");
        assert_eq!(note["annotations"]["audience"], json!(["assistant"]));
        assert!(
            note["text"]
                .as_str()
                .unwrap()
                .starts_with("Optional feedback request.")
        );
        assert_eq!(offer.feedback_id.len(), 32);
        assert!(id_of(&offer).len() <= token::MAX_ID);
    }
}
