use crate::client::InnerClient;
use crate::codec::FrontendMessage;
use crate::connection::RequestMessages;
use crate::types::BorrowToSql;
use crate::{Error, Portal, Statement, query};
use postgres_protocol::message::backend::Message;
use postgres_protocol::message::frontend;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

pub async fn bind<P, I>(
    client: &Arc<InnerClient>,
    statement: Statement,
    params: I,
) -> Result<Portal, Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = P>,
    I::IntoIter: ExactSizeIterator,
{
    bind_with_result_format(client, statement, params, query::FORMAT_BINARY).await
}

// --- pghacker-mcp fork edit (4/5) ---
/// Like [`bind`], but lets the caller choose the result-column format code
/// (`query::FORMAT_BINARY` or `query::FORMAT_TEXT`) for the portal.
///
/// A portal bound with `FORMAT_TEXT` yields rows whose column buffers hold the
/// server's text rendering, so they must be read with a decoder that expects
/// text — the ordinary `FromSql` implementations assume binary and would
/// misparse them.
pub async fn bind_with_result_format<P, I>(
    client: &Arc<InnerClient>,
    statement: Statement,
    params: I,
    result_format: i16,
) -> Result<Portal, Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = P>,
    I::IntoIter: ExactSizeIterator,
{
    let name = format!("p{}", NEXT_ID.fetch_add(1, Ordering::SeqCst));
    let buf = client.with_buf(|buf| {
        query::encode_bind(&statement, params, &name, result_format, buf)?;
        frontend::sync(buf);
        Ok(buf.split().freeze())
    })?;

    let mut responses = client.send(RequestMessages::Single(FrontendMessage::Raw(buf)))?;

    match responses.next().await? {
        Message::BindComplete => {}
        _ => return Err(Error::unexpected_message()),
    }

    Ok(Portal::new(client, name, statement))
}
