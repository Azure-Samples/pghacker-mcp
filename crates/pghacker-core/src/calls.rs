// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Tool calls that may be abandoned. When a call's client goes away, the
//! server drops the call's future; but a statement that the call was waiting
//! on would still run on the database, and its connection would go back to
//! the pool still busy with it. A call wrapped in [`abandonable`] that is
//! dropped before it completes cancels instead the statements on the
//! connections it holds from a [`CallPool`], and closes those connections, as
//! the query tool does with its own.
//!
//! Outside such a call (jobs, tests, the command line), a [`CallPool`] is a
//! plain pool that waits a bounded time for a connection.

use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use deadpool_postgres::{ClientWrapper, Object, Pool};
use tokio::task::futures::TaskLocalFuture;

use crate::db;

tokio::task_local! {
    static CALL: Arc<Call>;
}

/// What a call's connections need to know: whether it was abandoned.
#[derive(Debug, Default)]
struct Call {
    abandoned: AtomicBool,
}

/// `future` as a call whose connections from a [`CallPool`] have their
/// statements cancelled and are closed if it's dropped before it completes.
pub fn abandonable<F: Future>(future: F) -> Abandonable<F> {
    let call = Arc::new(Call::default());
    Abandonable {
        future: Some(Box::pin(CALL.scope(call.clone(), future))),
        call,
    }
}

/// The future of [`abandonable`].
pub struct Abandonable<F: Future> {
    future: Option<Scoped<F>>,
    call: Arc<Call>,
}

/// A call's future, with its [`Call`] in the task-local scope.
type Scoped<F> = Pin<Box<TaskLocalFuture<Arc<Call>, F>>>;

impl<F: Future> Future for Abandonable<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let this = self.get_mut();
        let future = this.future.as_mut().expect("polled after it completed");
        let output = ready!(future.as_mut().poll(cx));
        this.future = None;
        Poll::Ready(output)
    }
}

impl<F: Future> Drop for Abandonable<F> {
    fn drop(&mut self) {
        if self.future.is_some() {
            // The mark first: the call's connections, dropped with its
            // future next, read it.
            self.call.abandoned.store(true, Ordering::Release);
            self.future = None;
        }
    }
}

/// A pool for calls' connections: it waits at most `wait` for one, and the
/// connections it hands out within an [`abandonable`] call are cancelled and
/// closed if the call is abandoned.
#[derive(Clone)]
pub struct CallPool {
    pool: Pool,
    wait: Duration,
}

impl CallPool {
    pub fn new(pool: Pool, wait: Duration) -> Self {
        Self { pool, wait }
    }

    /// The pool underneath.
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// A connection, or an error once `wait` has passed with every one busy,
    /// or when the database can't be reached.
    pub async fn get(&self) -> Result<CallClient> {
        let object = match tokio::time::timeout(self.wait, self.pool.get()).await {
            Ok(object) => object.context("the database is unavailable")?,
            Err(_) => {
                return Err(anyhow!(
                    "all {} database connections for tool calls are busy; try again shortly",
                    self.pool.status().max_size
                ));
            }
        };
        Ok(CallClient {
            object: Some(object),
            call: CALL.try_with(Arc::clone).ok(),
        })
    }
}

/// A connection from a [`CallPool`]. It goes back to the pool when dropped,
/// unless its call was abandoned or is panicking: then its statement is
/// cancelled and it's closed.
pub struct CallClient {
    object: Option<Object>,
    call: Option<Arc<Call>>,
}

impl Deref for CallClient {
    type Target = ClientWrapper;

    fn deref(&self) -> &ClientWrapper {
        self.object.as_deref().expect("a connection until dropped")
    }
}

impl DerefMut for CallClient {
    fn deref_mut(&mut self) -> &mut ClientWrapper {
        self.object
            .as_deref_mut()
            .expect("a connection until dropped")
    }
}

impl Drop for CallClient {
    fn drop(&mut self) {
        let Some(object) = self.object.take() else {
            return;
        };
        let abandoned = self
            .call
            .as_ref()
            .is_some_and(|call| call.abandoned.load(Ordering::Acquire));
        // A call that panics drops its connections as it unwinds, before its
        // future is dropped as abandoned: they may still be busy with a
        // statement.
        if !abandoned && !std::thread::panicking() {
            return;
        }
        let cancel = object.cancel_token();
        let client = Object::take(object);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            if let Err(error) = db::cancel_query(&cancel).await {
                tracing::debug!(error = %format!("{error:#}"), "couldn't cancel an abandoned call's statement");
            }
            drop(client);
        });
    }
}
