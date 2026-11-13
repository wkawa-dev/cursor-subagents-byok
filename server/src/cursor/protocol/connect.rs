//! Encodes and decodes Connect protocol frames.
use bytes::{BufMut, Bytes, BytesMut};
use prost::Message;
use serde::Serialize;

use crate::{Error, Result};

pub const END_STREAM_FLAG: u8 = 0x02;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectCode {
    Canceled,
    InvalidArgument,
    NotFound,
    Unavailable,
    Internal,
}

impl ConnectCode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Canceled => "canceled",
            Self::InvalidArgument => "invalid_argument",
            Self::NotFound => "not_found",
            Self::Unavailable => "unavailable",
            Self::Internal => "internal",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConnectErrorDetail {
    #[serde(rename = "type")]
    pub type_name: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectStreamError {
    pub code: ConnectCode,
    pub message: String,
    pub details: Vec<ConnectErrorDetail>,
}

#[derive(Serialize)]
struct EndStreamResponse<'a> {
    error: WireError<'a>,
}

#[derive(Serialize)]
struct WireError<'a> {
    code: &'static str,
    #[serde(skip_serializing_if = "str::is_empty")]
    message: &'a str,
    #[serde(skip_serializing_if = "details_are_empty")]
    details: &'a [ConnectErrorDetail],
}

fn details_are_empty(details: &&[ConnectErrorDetail]) -> bool {
    details.is_empty()
}

pub fn encode_message<M: Message>(message: &M) -> Result<Bytes> {
    let len = message.encoded_len();
    let mut output = BytesMut::with_capacity(5 + len);
    output.put_u8(0);
    output.put_u32(len as u32);
    message.encode(&mut output)?;
    Ok(output.freeze())
}

pub fn encode_end_stream() -> Bytes {
    encode_end_stream_payload(b"{}")
}

pub fn encode_error_end_stream(error: &ConnectStreamError) -> Result<Bytes> {
    let payload = serde_json::to_vec(&EndStreamResponse {
        error: WireError {
            code: error.code.as_str(),
            message: &error.message,
            details: &error.details,
        },
    })?;
    Ok(encode_end_stream_payload(&payload))
}

fn encode_end_stream_payload(payload: &[u8]) -> Bytes {
    let mut output = BytesMut::with_capacity(5 + payload.len());
    output.put_u8(END_STREAM_FLAG);
    output.put_u32(payload.len() as u32);
    output.extend_from_slice(payload);
    output.freeze()
}

pub fn decode_unary<M: Message + Default>(body: &[u8]) -> Result<M> {
    if body.len() >= 5 {
        let flags = body[0];
        let length = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
        if flags & END_STREAM_FLAG == 0 && length == body.len() - 5 {
            return Ok(M::decode(&body[5..])?);
        }
    }
    Ok(M::decode(body)?)
}

pub fn decode_frames(body: &[u8]) -> Result<Vec<(u8, Bytes)>> {
    let mut frames = Vec::new();
    let mut decoder = FrameDecoder::new();
    decoder.push(body);
    while let Some((flags, payload, _)) = decoder.next_frame()? {
        frames.push((flags, payload));
    }
    if !decoder.unconsumed().is_empty() {
        return Err(Error::Protocol("truncated Connect envelope".into()));
    }
    Ok(frames)
}

/// Upper bound for a single Connect frame payload. Anything larger is treated
/// as a desynchronized stream rather than buffered unboundedly.
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;

/// Incremental parser for Connect streaming envelopes (`flag + len + payload`).
/// Feeding request-body chunks into [`FrameDecoder::push`] and draining
/// [`FrameDecoder::next_frame`] keeps long-lived bidirectional streams (e.g.
/// `agent.v1.AgentService/Run`) flowing without buffering the whole body.
pub struct FrameDecoder {
    buf: BytesMut,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self {
            buf: BytesMut::new(),
        }
    }
    pub fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// Returns the next complete frame as `(flags, payload, raw)` where `raw`
    /// covers the full envelope bytes, or `None` when more data is needed.
    pub fn next_frame(&mut self) -> Result<Option<(u8, Bytes, Bytes)>> {
        if self.buf.len() < 5 {
            return Ok(None);
        }
        let flags = self.buf[0];
        let length =
            u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if length > MAX_FRAME_LEN {
            return Err(Error::Protocol(format!(
                "Connect frame too large ({length} bytes)"
            )));
        }
        if self.buf.len() < 5 + length {
            return Ok(None);
        }
        let raw = self.buf.split_to(5 + length).freeze();
        let payload = raw.slice(5..);
        Ok(Some((flags, payload, raw)))
    }

    /// Bytes received but not yet consumed by [`FrameDecoder::next_frame`]
    /// (e.g. a partial trailing frame). Needed to re-chain a stream that was
    /// only peeked at before forwarding it upstream untouched.
    pub fn take_unconsumed(&mut self) -> Bytes {
        self.buf.split().freeze()
    }

    pub fn unconsumed(&self) -> &[u8] {
        &self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_decoder_reassembles_chunked_stream() {
        let first = encode_message(&proto_stub(b"hello")).unwrap();
        let second = encode_message(&proto_stub(b"world!")).unwrap();
        let mut wire = BytesMut::new();
        wire.extend_from_slice(&first);
        wire.extend_from_slice(&second);
        let wire = wire.freeze();

        let mut decoder = FrameDecoder::new();
        // Feed one byte at a time like a slow H1 upload would arrive.
        let mut frames = Vec::new();
        for byte in wire.iter() {
            decoder.push(std::slice::from_ref(byte));
            while let Some((flags, payload, _)) = decoder.next_frame().unwrap() {
                assert_eq!(flags, 0);
                frames.push(payload);
            }
        }
        assert!(decoder.unconsumed().is_empty());
        assert_eq!(frames.len(), 2);
        let first = StubMessage::decode(frames[0].as_ref()).unwrap();
        let second = StubMessage::decode(frames[1].as_ref()).unwrap();
        assert_eq!(first.payload, b"hello");
        assert_eq!(second.payload, b"world!");
    }

    #[test]
    fn frame_decoder_reports_truncated_envelope() {
        let mut decoder = FrameDecoder::new();
        decoder.push(&[0, 0, 0, 0, 10, 1, 2, 3]);
        assert!(decoder.next_frame().unwrap().is_none());
        assert_eq!(decoder.unconsumed().len(), 8);
    }

    #[test]
    fn decode_frames_round_trips() {
        let mut wire = BytesMut::new();
        wire.extend_from_slice(&encode_message(&proto_stub(b"a")).unwrap());
        wire.extend_from_slice(&encode_end_stream());
        let frames = decode_frames(&wire).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].0 & END_STREAM_FLAG, END_STREAM_FLAG);
    }

    // Minimal prost message stub: field 1, length-delimited bytes.
    fn proto_stub(data: &[u8]) -> StubMessage {
        StubMessage {
            payload: data.to_vec(),
        }
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    struct StubMessage {
        #[prost(bytes, tag = "1")]
        payload: Vec<u8>,
    }
}
