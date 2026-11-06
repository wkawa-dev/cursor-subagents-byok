//! Serves the bidirectional `agent.v1.AgentService/Run` RPC introduced for
//! Cursor agents (replaces the `BidiAppend` + `RunSSE` pair with a single
//! `stream AgentClientMessage` ↔ `stream AgentServerMessage` exchange).
//!
//! The wire types are identical to the legacy flow, so upstream messages are
//! bridged into the same [`bidi::append`] pipeline with synthesized sequence
//! numbers (a single HTTP/1.1 Connect stream is already ordered), and the
//! subscribed transport output is streamed back as Connect frames.

use axum::{
    body::Body,
    extract::{Extension, State},
    http::{header, HeaderValue, Request, Response, StatusCode},
};
use bytes::Bytes;
use prost::Message;
use std::convert::Infallible;
use tokio_stream::StreamExt;

use super::{
    bidi,
    handlers::{is_local_model, parent_headers},
    proxy::{self, CursorProxy},
    run_sse::{self, LocalRunGuard, TraceStreamSink},
};
use crate::{
    cursor::{
        protocol::{
            connect::{FrameDecoder, END_STREAM_FLAG},
            proto::agent::v1 as agent,
        },
        transport::{TransportParent, TransportRegistry},
    },
    Error, Result,
};

/// Polling variant of the agent stream. No local handler exists; forwarding
/// keeps native behavior while making the traffic visible in backend logs
/// (transparent MITM passthrough would be silent).
pub async fn run_poll_handler(
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    tracing::info!(
        path = request.uri().path(),
        "forwarding Cursor agent poll to upstream"
    );
    proxy::forward(Extension(proxy), request).await
}

pub async fn run_handler(
    State(registry): State<TransportRegistry>,
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let (parts, body) = request.into_parts();
    let parent = parent_headers(&parts.headers)?;
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    tracing::info!(content_type, "Run bidi stream opened");

    let mut stream = body.into_data_stream();
    let mut decoder = FrameDecoder::new();
    let first_raw: Bytes;
    let first_message: agent::AgentClientMessage;
    loop {
        match stream.next().await {
            None => {
                return Err(Error::Protocol(
                    "Run stream closed before the first message".into(),
                ));
            }
            Some(Err(error)) => {
                return Err(Error::Protocol(format!("Run request body failed: {error}")));
            }
            Some(Ok(chunk)) => {
                decoder.push(&chunk);
                if let Some((flags, payload, raw)) = decoder.next_frame()? {
                    reject_compressed(flags)?;
                    first_raw = raw;
                    first_message =
                        agent::AgentClientMessage::decode(payload.as_ref()).map_err(|error| {
                            Error::Protocol(format!("invalid Run AgentClientMessage: {error}"))
                        })?;
                    break;
                }
            }
        }
    }

    let probe = bidi::DecodedAppend {
        request_id: String::new(),
        seqno: 0,
        message: first_message,
    };
    let agent::agent_client_message::Message::RunRequest(run_request) = probe
        .message
        .message
        .as_ref()
        .ok_or_else(|| Error::Protocol("first Run message must be a RunRequest".into()))?
    else {
        return Err(Error::Protocol(
            "first Run message must select a model".into(),
        ));
    };
    let Some(model_id) = probe.model_id().map(str::to_owned) else {
        return Err(Error::Protocol(
            "first Run message must select a model".into(),
        ));
    };

    // One connection carries both directions, so no cross-stream correlation
    // key exists. Synthesize a unique transport key instead; the stable
    // conversation identity still flows via set_conversation_id downstream.
    let request_id = run_request
        .run_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("run-bidi-{}", uuid::Uuid::new_v4()));
    let conversation_id = probe.conversation_id().map(str::to_owned);
    let trace_metadata = probe.trace_metadata();

    let trace = registry.trace(&request_id);
    let local = is_local_model(&registry, &model_id).await?;
    if local {
        tracing::info!(%request_id, %model_id, "routing Cursor Run stream to BYOK provider");
    } else {
        tracing::info!(%request_id, %model_id, "routing Cursor Run stream to Cursor upstream");
    }
    trace.begin(
        conversation_id.as_deref(),
        if local {
            "local_byok"
        } else {
            "cursor_official"
        },
        Some(model_id.as_str()),
    );

    if !local {
        trace.request(
            "run_request",
            first_raw.clone(),
            serde_json::json!({"route_outcome": "upstream", "accepted": true, "run": trace_metadata}),
        );
        let chained = chain_with_prefix(first_raw, decoder.take_unconsumed(), stream);
        return proxy::forward(
            Extension(proxy),
            Request::from_parts(parts, Body::from_stream(chained)),
        )
        .await;
    }

    trace.request(
        "run_request",
        first_raw,
        serde_json::json!({"route_outcome": "local", "accepted": true, "run": trace_metadata}),
    );
    bidi::append(
        &registry,
        bidi::DecodedAppend {
            request_id: request_id.clone(),
            seqno: 0,
            message: probe.message,
        },
        parent.clone(),
    )
    .await?;

    let handle = registry
        .local(&request_id)
        .await
        .ok_or_else(|| Error::Protocol("Run transport missing after first append".into()))?;
    let mut receiver = handle.subscribe();
    let trace = handle.trace().cloned();
    if let Some(trace) = &trace {
        trace.response_started(StatusCode::OK.as_u16());
    }

    let pump_registry = registry.clone();
    let pump_id = request_id.clone();
    let pump_cancel = handle.disconnect_token();
    let pump = tokio::spawn(async move {
        pump_appends(pump_registry, pump_id, parent, stream, pump_cancel).await;
    });

    let body_stream = async_stream::stream! {
        let _pump = PumpGuard::new(pump);
        let mut guard = LocalRunGuard::new(handle);
        let mut trace = TraceStreamSink::new(trace, "byok_server");
        while let Some(chunk) = receiver.recv().await {
            let terminal = is_end_stream_frame(&chunk);
            trace.chunk(&chunk);
            if terminal {
                guard.complete();
                trace.finish(run_sse::end_stream_error(&chunk));
            }
            yield Ok::<Bytes, Infallible>(chunk);
            if terminal {
                return;
            }
        }
        guard.complete();
        trace.finish(None);
    };

    let mut response = Response::new(Body::from_stream(body_stream));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/connect+proto"),
    );
    response
        .headers_mut()
        .insert("connect-protocol-version", HeaderValue::from_static("1"));
    Ok(response)
}

fn reject_compressed(flags: u8) -> Result<()> {
    if flags & 0x01 != 0 {
        return Err(Error::Protocol(
            "compressed Connect frames are not supported".into(),
        ));
    }
    Ok(())
}

fn is_end_stream_frame(frame: &Bytes) -> bool {
    frame
        .first()
        .is_some_and(|flags| flags & END_STREAM_FLAG != 0)
}

/// Prefixes already-consumed raw bytes back in front of a request body stream
/// so a stream that was only peeked at can be forwarded upstream untouched.
fn chain_with_prefix(
    prefix: Bytes,
    leftover: Bytes,
    stream: impl tokio_stream::Stream<Item = std::result::Result<Bytes, axum::Error>> + Send + 'static,
) -> impl tokio_stream::Stream<Item = std::result::Result<Bytes, axum::Error>> {
    async_stream::stream! {
        yield Ok::<Bytes, axum::Error>(prefix);
        if !leftover.is_empty() {
            yield Ok::<Bytes, axum::Error>(leftover);
        }
        tokio::pin!(stream);
        while let Some(chunk) = stream.next().await {
            yield chunk;
        }
    }
}

async fn pump_appends(
    registry: TransportRegistry,
    request_id: String,
    parent: Option<TransportParent>,
    stream: impl tokio_stream::Stream<Item = std::result::Result<Bytes, axum::Error>> + Unpin,
    cancel: tokio_util::sync::CancellationToken,
) {
    let mut stream = stream;
    let mut decoder = FrameDecoder::new();
    let mut seqno: i64 = 1;
    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => break,
            next = stream.next() => next,
        };
        let Some(next) = chunk else { break };
        let chunk = match next {
            Ok(chunk) => chunk,
            Err(error) => {
                tracing::error!(%request_id, %error, "Run request body failed; disconnecting transport");
                break;
            }
        };
        decoder.push(&chunk);
        let frames = match drain_frames(&mut decoder) {
            Ok(frames) => frames,
            Err(error) => {
                tracing::error!(%request_id, %error, "Run stream desynchronized; disconnecting transport");
                break;
            }
        };
        let mut failed = false;
        for (flags, payload) in frames {
            if flags & END_STREAM_FLAG != 0 {
                tracing::debug!(%request_id, "Run client half-closed its stream");
                return;
            }
            if let Err(error) = reject_compressed(flags) {
                tracing::error!(%request_id, %error, "Run stream uses unsupported framing");
                failed = true;
                break;
            }
            let message = match agent::AgentClientMessage::decode(payload.as_ref()) {
                Ok(message) => message,
                Err(error) => {
                    tracing::error!(%request_id, %error, "invalid Run AgentClientMessage");
                    failed = true;
                    break;
                }
            };
            if let Err(error) = bidi::append(
                &registry,
                bidi::DecodedAppend {
                    request_id: request_id.clone(),
                    seqno,
                    message,
                },
                parent.clone(),
            )
            .await
            {
                tracing::error!(%request_id, seqno, %error, "Run append rejected");
                failed = true;
                break;
            }
            seqno = seqno.saturating_add(1);
        }
        if failed {
            break;
        }
    }
    if let Some(handle) = registry.local(&request_id).await {
        handle.disconnect().await;
    }
}

fn drain_frames(decoder: &mut FrameDecoder) -> Result<Vec<(u8, Bytes)>> {
    let mut frames = Vec::new();
    while let Some((flags, payload, _)) = decoder.next_frame()? {
        frames.push((flags, payload));
    }
    Ok(frames)
}

struct PumpGuard {
    pump: tokio::task::JoinHandle<()>,
}

impl PumpGuard {
    fn new(pump: tokio::task::JoinHandle<()>) -> Self {
        Self { pump }
    }
}

impl Drop for PumpGuard {
    fn drop(&mut self) {
        self.pump.abort();
    }
}
