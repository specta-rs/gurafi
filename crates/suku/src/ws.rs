//! GraphQL over WebSocket (`graphql-transport-ws`).
//!
//! This module implements the [GraphQL over WebSocket protocol][spec] used by
//! `graphql-ws`/`graphql-transport-ws` clients. It is layered the same way as the rest of `suku`:
//! [`serve`] drives one connection over a stream of text messages and a channel of outgoing
//! messages, so it can be tested without a socket and embedded in a non-Axum server. The Axum
//! integration calls it after an upgrade; see [`crate::axum::GraphQL::websocket`].
//!
//! # Connection lifecycle
//!
//! 1. The client sends `connection_init`.
//! 2. The server replies `connection_ack` (or closes `4401`/`4408`/`4429`).
//! 3. The client sends `subscribe` messages, each with a unique `id` and a GraphQL request payload.
//! 4. Queries and mutations produce one `next` followed by `complete`; subscriptions produce
//!    `next` per event followed by `complete`. Request/validation failures produce `error` and no
//!    `complete`.
//! 5. Either side may send `complete` for an id. A client may reuse an id once the operation has
//!    finished.
//!
//! JSON-level `ping`/`pong` messages are supported; WebSocket-level ping frames are handled by the
//! Axum adapter.
//!
//! # Authentication
//!
//! The context is extracted **once per connection**, from the HTTP upgrade request (headers,
//! cookies and middleware extensions), before the socket is accepted. Every subscription on that
//! connection reuses the same [`Context`]. Nothing in `connection_init` or `subscribe` can widen
//! it, which is what stops a client from bypassing the authentication that guarded the upgrade.
//! Authenticate per operation inside resolvers/`Authorizer` when credentials can expire mid
//! connection.
//!
//! WebSocket handshakes are not subject to a CORS preflight and browsers attach cookies to them.
//! If the extractor authenticates with cookies, it must also validate the `Origin` header (or the
//! application must require a token); the HTTP restriction that `GET` may only execute queries
//! does not apply to messages sent after the upgrade.
//!
//! # Limits
//!
//! Every operation goes through [`Server::execute`]/[`Server::subscribe`], so depth, complexity,
//! breadth, error, timeout and concurrency limits apply exactly as they do over HTTP. In addition
//! this transport enforces:
//!
//! - [`WsConfig::max_message_bytes`], defaulting to the server's body limit;
//! - [`WsConfig::max_operations`], bounding concurrently active operations per connection;
//! - [`WsConfig::connection_init_timeout`], closing `4408` when the client never initialises.
//!
//! [spec]: https://github.com/enisdenjo/graphql-ws/blob/master/PROTOCOL.md

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use futures::{Stream, StreamExt as _};
use gql::{Response, ast::OperationKind};
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};
use tokio::{sync::mpsc, task::AbortHandle};

use crate::{
    Context, GraphQLRequest, Server,
    error::{ErrorKind, ResponseMeta},
    response::ResponseExt as _,
};

/// The WebSocket subprotocol implemented by this module.
pub const GRAPHQL_TRANSPORT_WS: &str = "graphql-transport-ws";

/// `4400`: the message was not valid JSON or not a valid protocol message.
pub const CLOSE_BAD_REQUEST: u16 = 4400;
/// `4401`: an operation was sent before `connection_ack`.
pub const CLOSE_UNAUTHORIZED: u16 = 4401;
/// `4403`: the connection was refused (for example by authentication).
pub const CLOSE_FORBIDDEN: u16 = 4403;
/// `4408`: no `connection_init` arrived within the configured timeout.
pub const CLOSE_INIT_TIMEOUT: u16 = 4408;
/// `4409`: a `subscribe` reused an id that is still active.
pub const CLOSE_SUBSCRIBER_EXISTS: u16 = 4409;
/// `4429`: the client sent more than one `connection_init`.
pub const CLOSE_TOO_MANY_INIT: u16 = 4429;

/// A message delivered to [`serve`] by the transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WsInput {
    /// A text frame containing a protocol message.
    Text(String),
    /// The peer closed the connection.
    Close,
    /// A frame that cannot carry the protocol, such as a binary frame.
    Invalid(String),
}

/// A message [`serve`] asks the transport to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WsOutput {
    /// A text frame containing a protocol message.
    Text(String),
    /// Close the socket with a protocol status code.
    Close {
        /// A `4400`-series application close code.
        code: u16,
        /// A short, client-safe reason.
        reason: String,
    },
}

/// WebSocket transport limits.
#[derive(Clone, Debug)]
pub struct WsConfig {
    /// How long to wait for `connection_init`. `None` waits forever.
    ///
    /// Defaults to 10 seconds.
    pub connection_init_timeout: Option<Duration>,
    /// Maximum concurrent operations (queries, mutations and subscriptions) per connection.
    ///
    /// `0` disables the limit. Defaults to 32.
    pub max_operations: usize,
    /// Maximum size of one protocol message in bytes.
    ///
    /// Defaults to the server's `max_body_bytes`; `0` disables the check.
    pub max_message_bytes: Option<usize>,
}

impl Default for WsConfig {
    fn default() -> Self {
        Self {
            connection_init_timeout: Some(Duration::from_secs(10)),
            max_operations: 32,
            max_message_bytes: None,
        }
    }
}

impl WsConfig {
    /// Create the default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the `connection_init` timeout. `None` waits forever.
    #[must_use]
    pub fn connection_init_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.connection_init_timeout = timeout;
        self
    }

    /// Set the per-connection active-operation limit. `0` disables it.
    #[must_use]
    pub fn max_operations(mut self, operations: usize) -> Self {
        self.max_operations = operations;
        self
    }

    /// Override the maximum protocol message size. `0` disables the check.
    #[must_use]
    pub fn max_message_bytes(mut self, bytes: usize) -> Self {
        self.max_message_bytes = Some(bytes);
        self
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    ConnectionInit,
    Ping { payload: Option<JsonValue> },
    Pong,
    Subscribe { id: String, payload: GraphQLRequest },
    Complete { id: String },
}

/// Drive one WebSocket connection to completion.
///
/// `incoming` yields text messages from the client; `outgoing` receives text and close messages for
/// the transport to write. The future returns when the client disconnects, the transport closes,
/// or a fatal protocol error was sent.
///
/// The caller must authenticate before calling this function and pass the resulting [`Context`];
/// see the module documentation for why authentication is per connection.
///
/// # Panics
///
/// Spawns one Tokio task per active operation. It does not panic itself; a resolver panic aborts
/// only that operation, and the connection's bookkeeping is cleaned up by a drop guard.
pub async fn serve<S>(
    server: Server,
    config: WsConfig,
    context: Context,
    mut incoming: S,
    outgoing: mpsc::Sender<WsOutput>,
) where
    S: Stream<Item = WsInput> + Unpin + Send + 'static,
{
    let max_message_bytes = config
        .max_message_bytes
        .unwrap_or_else(|| server.max_body_bytes_limit());

    // Phase 1: wait for `connection_init`.
    let initialised = match config.connection_init_timeout {
        Some(timeout) => match tokio::time::timeout(
            timeout,
            await_connection_init(&mut incoming, max_message_bytes, &outgoing),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                close(
                    &outgoing,
                    CLOSE_INIT_TIMEOUT,
                    "Connection initialisation timeout",
                )
                .await;
                return;
            }
        },
        None => await_connection_init(&mut incoming, max_message_bytes, &outgoing).await,
    };
    match initialised {
        Ok(true) => {}
        // The peer disconnected, or a protocol close was already sent.
        Ok(false) | Err(()) => return,
    }

    if !send_json(&outgoing, json!({ "type": "connection_ack" })).await {
        return;
    }
    run_connection(
        server,
        config,
        context,
        incoming,
        outgoing,
        max_message_bytes,
    )
    .await;
}

/// Wait for `connection_init`, handling pings and rejecting operations before the handshake.
///
/// Returns `Ok(true)` after acknowledging the connection, `Ok(false)` when the peer disconnected,
/// and `Err(())` when a fatal protocol close was sent.
async fn await_connection_init<S>(
    incoming: &mut S,
    max_message_bytes: usize,
    outgoing: &mpsc::Sender<WsOutput>,
) -> Result<bool, ()>
where
    S: Stream<Item = WsInput> + Unpin,
{
    loop {
        let Some(input) = incoming.next().await else {
            return Ok(false);
        };
        let text = match input {
            WsInput::Close => return Ok(false),
            WsInput::Invalid(reason) => {
                close(outgoing, CLOSE_BAD_REQUEST, &reason).await;
                return Err(());
            }
            WsInput::Text(text) => text,
        };
        if too_large(&text, max_message_bytes) {
            close(
                outgoing,
                CLOSE_BAD_REQUEST,
                "Protocol message exceeds the configured size limit",
            )
            .await;
            return Err(());
        }
        let message = match parse_message(&text) {
            Ok(message) => message,
            Err(()) => {
                close(outgoing, CLOSE_BAD_REQUEST, "Invalid protocol message").await;
                return Err(());
            }
        };
        match message {
            ClientMessage::ConnectionInit => return Ok(true),
            ClientMessage::Ping { payload } => send_pong(outgoing, payload).await,
            ClientMessage::Pong => {}
            ClientMessage::Subscribe { .. } => {
                close(
                    outgoing,
                    CLOSE_UNAUTHORIZED,
                    "Connection has not been acknowledged",
                )
                .await;
                return Err(());
            }
            // The protocol tells both sides to ignore a `complete` for an operation they
            // consider completed. Before `connection_ack` there is no operation to complete, so
            // ignore it rather than treating a well-formed message as malformed.
            ClientMessage::Complete { .. } => {}
        }
    }
}

struct ActiveOperation {
    sequence: u64,
    handle: AbortHandle,
}

struct Done {
    id: String,
    sequence: u64,
}

async fn run_connection<S>(
    server: Server,
    config: WsConfig,
    context: Context,
    mut incoming: S,
    outgoing: mpsc::Sender<WsOutput>,
    max_message_bytes: usize,
) where
    S: Stream<Item = WsInput> + Unpin + Send + 'static,
{
    let active_ids: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let mut tasks: HashMap<String, ActiveOperation> = HashMap::new();
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<Done>();
    let mut sequence = 0u64;

    loop {
        tokio::select! {
            biased;
            done = done_rx.recv() => {
                if let Some(done) = done {
                    if tasks
                        .get(&done.id)
                        .is_some_and(|active| active.sequence == done.sequence)
                    {
                        tasks.remove(&done.id);
                    }
                }
            }
            input = incoming.next() => {
                let Some(input) = input else {
                    break;
                };
                let text = match input {
                    WsInput::Close => break,
                    WsInput::Invalid(reason) => {
                        close(&outgoing, CLOSE_BAD_REQUEST, &reason).await;
                        break;
                    }
                    WsInput::Text(text) => text,
                };
                if too_large(&text, max_message_bytes) {
                    close(
                        &outgoing,
                        CLOSE_BAD_REQUEST,
                        "Protocol message exceeds the configured size limit",
                    )
                    .await;
                    break;
                }
                let message = match parse_message(&text) {
                    Ok(message) => message,
                    Err(()) => {
                        close(&outgoing, CLOSE_BAD_REQUEST, "Invalid protocol message").await;
                        break;
                    }
                };
                match message {
                    ClientMessage::ConnectionInit => {
                        close(
                            &outgoing,
                            CLOSE_TOO_MANY_INIT,
                            "Too many initialisation requests",
                        )
                        .await;
                        break;
                    }
                    ClientMessage::Ping { payload } => send_pong(&outgoing, payload).await,
                    ClientMessage::Pong => {}
                    ClientMessage::Subscribe { id, payload } => {
                        if id.is_empty() {
                            close(&outgoing, CLOSE_BAD_REQUEST, "Operation id must not be empty")
                                .await;
                            break;
                        }
                        let admitted = {
                            let mut ids = lock_ids(&active_ids);
                            if ids.contains(&id) {
                                None
                            } else if config.max_operations != 0
                                && ids.len() >= config.max_operations
                            {
                                Some(false)
                            } else {
                                ids.insert(id.clone());
                                Some(true)
                            }
                        };
                        match admitted {
                            None => {
                                close(
                                    &outgoing,
                                    CLOSE_SUBSCRIBER_EXISTS,
                                    "Subscriber for this id already exists",
                                )
                                .await;
                                break;
                            }
                            Some(false) => {
                                let mut response = Response::error(gql::GraphQLError::new(format!(
                                    "The connection already has {} active operations.",
                                    config.max_operations
                                )));
                                server.format_response(
                                    &mut response,
                                    ResponseMeta::none(),
                                    ErrorKind::Request,
                                );
                                send_error(&outgoing, &id, &response).await;
                            }
                            Some(true) => {
                                sequence += 1;
                                let sequence = sequence;
                                let handle = tokio::spawn(run_operation(
                                    OperationTask {
                                        server: server.clone(),
                                        request: payload,
                                        context: context.clone(),
                                        id: id.clone(),
                                        sequence,
                                        outgoing: outgoing.clone(),
                                        done: done_tx.clone(),
                                        active: Arc::clone(&active_ids),
                                    },
                                ));
                                tasks.insert(
                                    id,
                                    ActiveOperation {
                                        sequence,
                                        handle: handle.abort_handle(),
                                    },
                                );
                            }
                        }
                    }
                    ClientMessage::Complete { id } => {
                        if let Some(active) = tasks.remove(&id) {
                            active.handle.abort();
                        }
                        lock_ids(&active_ids).remove(&id);
                    }
                }
            }
        }
    }

    for active in tasks.into_values() {
        active.handle.abort();
    }
}

struct OperationTask {
    server: Server,
    request: GraphQLRequest,
    context: Context,
    id: String,
    sequence: u64,
    outgoing: mpsc::Sender<WsOutput>,
    done: mpsc::UnboundedSender<Done>,
    active: Arc<Mutex<HashSet<String>>>,
}

/// Removes the operation id and notifies the connection even if the task is aborted or panics.
struct FinishGuard {
    id: String,
    sequence: u64,
    outgoing: mpsc::Sender<WsOutput>,
    done: mpsc::UnboundedSender<Done>,
    active: Arc<Mutex<HashSet<String>>>,
    removed: bool,
    finished: bool,
}

impl FinishGuard {
    fn new(task: &OperationTask) -> Self {
        Self {
            id: task.id.clone(),
            sequence: task.sequence,
            outgoing: task.outgoing.clone(),
            done: task.done.clone(),
            active: Arc::clone(&task.active),
            removed: false,
            finished: false,
        }
    }

    fn remove_id(&mut self) {
        if !self.removed {
            lock_ids(&self.active).remove(&self.id);
            self.removed = true;
        }
    }

    async fn finish(mut self, final_message: Option<String>) {
        // Release the id before the final message reaches the client: once it observes
        // `complete`/`error` it is allowed to reuse it.
        self.remove_id();
        if let Some(message) = final_message {
            let _ = self.outgoing.send(WsOutput::Text(message)).await;
        }
        self.finished = true;
        let _ = self.done.send(Done {
            id: self.id.clone(),
            sequence: self.sequence,
        });
    }
}

impl Drop for FinishGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.remove_id();
        let _ = self.done.send(Done {
            id: self.id.clone(),
            sequence: self.sequence,
        });
    }
}

async fn run_operation(task: OperationTask) {
    let guard = FinishGuard::new(&task);
    let OperationTask {
        server,
        request,
        context,
        id,
        outgoing,
        ..
    } = task;

    let kind = match server.operation_kind(&request) {
        Ok(kind) => kind,
        Err(error) => {
            let mut response = Response::error(error);
            server.format_response(
                &mut response,
                ResponseMeta::from_request(&request),
                ErrorKind::Request,
            );
            let message = error_message(&id, &response);
            guard.finish(Some(message)).await;
            return;
        }
    };

    if matches!(kind, Some(OperationKind::Subscription)) {
        match server.subscribe(&request, context).await {
            Ok(subscription) => {
                let mut stream = subscription.stream;
                while let Some(response) = stream.next().await {
                    let message = next_message(&id, &response);
                    if outgoing.send(WsOutput::Text(message)).await.is_err() {
                        return;
                    }
                }
                guard.finish(Some(complete_message(&id))).await;
            }
            Err(response) => {
                let message = error_message(&id, &response);
                guard.finish(Some(message)).await;
            }
        }
        return;
    }

    let response = server.execute(&request, context).await;
    if is_request_error(&response) {
        // `error` terminates the operation; no `complete` follows it.
        let message = error_message(&id, &response);
        guard.finish(Some(message)).await;
    } else {
        if outgoing
            .send(WsOutput::Text(next_message(&id, &response)))
            .await
            .is_err()
        {
            return;
        }
        guard.finish(Some(complete_message(&id))).await;
    }
}

/// Whether a response is a request failure (no data and no response path) rather than an
/// execution result.
///
/// This mirrors the HTTP status rule: validation/parse/coercion errors become `error` messages,
/// while field errors are still delivered as a normal `next` payload.
fn is_request_error(response: &Response) -> bool {
    response.data.is_none()
        && response.has_errors()
        && response.errors.iter().all(|error| error.path().is_none())
}

fn parse_message(text: &str) -> Result<ClientMessage, ()> {
    serde_json::from_str(text).map_err(|_| ())
}

fn too_large(text: &str, max_message_bytes: usize) -> bool {
    max_message_bytes != 0 && text.len() > max_message_bytes
}

fn next_message(id: &str, response: &Response) -> String {
    json!({
        "id": id,
        "type": "next",
        "payload": response.to_json(),
    })
    .to_string()
}

fn error_message(id: &str, response: &Response) -> String {
    let errors =
        serde_json::to_value(&response.errors).unwrap_or_else(|_| JsonValue::Array(Vec::new()));
    json!({
        "id": id,
        "type": "error",
        "payload": errors,
    })
    .to_string()
}

async fn send_error(outgoing: &mpsc::Sender<WsOutput>, id: &str, response: &Response) {
    let _ = outgoing
        .send(WsOutput::Text(error_message(id, response)))
        .await;
}

fn complete_message(id: &str) -> String {
    json!({ "id": id, "type": "complete" }).to_string()
}

async fn send_pong(outgoing: &mpsc::Sender<WsOutput>, payload: Option<JsonValue>) {
    let message = match payload.filter(|payload| !payload.is_null()) {
        Some(payload) => json!({ "type": "pong", "payload": payload }),
        None => json!({ "type": "pong" }),
    };
    let _ = outgoing.send(WsOutput::Text(message.to_string())).await;
}

async fn send_json(outgoing: &mpsc::Sender<WsOutput>, value: JsonValue) -> bool {
    outgoing
        .send(WsOutput::Text(value.to_string()))
        .await
        .is_ok()
}

async fn close(outgoing: &mpsc::Sender<WsOutput>, code: u16, reason: &str) {
    let _ = outgoing
        .send(WsOutput::Close {
            code,
            reason: reason.to_owned(),
        })
        .await;
}

fn lock_ids(ids: &Mutex<HashSet<String>>) -> MutexGuard<'_, HashSet<String>> {
    ids.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Adapt an Axum WebSocket to [`serve`].
#[cfg(feature = "ws")]
pub(crate) async fn serve_axum(
    server: Server,
    config: WsConfig,
    context: Context,
    socket: ::axum::extract::ws::WebSocket,
) {
    use ::axum::extract::ws::{CloseFrame, Message};
    use futures::{SinkExt as _, StreamExt as _};

    let (mut sink, stream) = socket.split();
    let (outgoing, mut receiver) = mpsc::channel::<WsOutput>(64);

    let writer = tokio::spawn(async move {
        while let Some(output) = receiver.recv().await {
            match output {
                WsOutput::Text(text) => {
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                WsOutput::Close { code, reason } => {
                    let frame = CloseFrame {
                        code,
                        reason: reason.into(),
                    };
                    let _ = sink.send(Message::Close(Some(frame))).await;
                    let _ = sink.close().await;
                    break;
                }
            }
        }
    });

    let incoming = stream
        .filter_map(|message| async move {
            match message {
                Ok(Message::Text(text)) => Some(WsInput::Text(text.to_string())),
                Ok(Message::Binary(_)) => Some(WsInput::Invalid(
                    "Binary messages are not supported".to_owned(),
                )),
                Ok(Message::Close(_)) | Err(_) => Some(WsInput::Close),
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => None,
            }
        })
        .boxed();

    serve(server, config, context, incoming, outgoing).await;
    let _ = writer.await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_message_types() {
        assert!(parse_message(r#"{"type":"nope"}"#).is_err());
    }

    #[test]
    fn parses_a_subscribe_payload() {
        let message = parse_message(
            r#"{"id":"1","type":"subscribe","payload":{"query":"{ hello }","variables":{"x":1}}}"#,
        )
        .expect("message");
        match message {
            ClientMessage::Subscribe { id, payload } => {
                assert_eq!(id, "1");
                assert_eq!(payload.query, "{ hello }");
            }
            _ => panic!("expected subscribe"),
        }
    }
}
