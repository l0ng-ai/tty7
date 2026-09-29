//! JSON-RPC 2.0 as a language server speaks it over stdio: every message is a
//! `Content-Length` header block followed by exactly that many bytes of JSON.
//!
//! Nothing here touches a process or a thread, so the framing — the part a
//! pipe hands over in arbitrary pieces — can be tested byte by byte.

use serde_json::{Value, json};

/// The id tty7 gives its own requests. A server's requests to us may carry a
/// string id instead; those are echoed back as the raw [`Value`].
pub(crate) type RequestId = i64;

/// Frames one message for the wire.
pub(crate) fn encode(message: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(message).unwrap_or_default();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(&body);
    out
}

pub(crate) fn request(id: RequestId, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

pub(crate) fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

pub(crate) fn response(id: Value, result: Result<Value, ResponseError>) -> Value {
    match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(e) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": e.code, "message": e.message },
        }),
    }
}

/// The `error` member of a response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResponseError {
    pub(crate) code: i64,
    pub(crate) message: String,
}

impl ResponseError {
    pub(crate) const METHOD_NOT_FOUND: i64 = -32601;

    pub(crate) fn method_not_found(method: &str) -> Self {
        Self {
            code: Self::METHOD_NOT_FOUND,
            message: format!("tty7 does not handle {method}"),
        }
    }
}

impl std::fmt::Display for ResponseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for ResponseError {}

/// One message from the server, sorted by what it asks of us.
#[derive(Debug, PartialEq)]
pub(crate) enum Incoming {
    /// The answer to one of our requests.
    Response {
        id: RequestId,
        result: Result<Value, ResponseError>,
    },
    /// The server asking us something; it waits for a [`response`].
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
}

/// Sorts a decoded body. `None` for anything that is not a message this
/// client can act on — a response to an id it never sent, a batch, garbage.
pub(crate) fn classify(body: &[u8]) -> Option<Incoming> {
    let mut value: Value = serde_json::from_slice(body).ok()?;
    let obj = value.as_object_mut()?;
    let method = obj.get("method").and_then(Value::as_str).map(str::to_owned);
    let params = obj.remove("params").unwrap_or(Value::Null);
    match (method, obj.remove("id")) {
        (Some(method), Some(id)) if !id.is_null() => Some(Incoming::Request { id, method, params }),
        (Some(method), _) => Some(Incoming::Notification { method, params }),
        (None, Some(id)) => {
            let id = id.as_i64()?;
            let result = match obj.remove("error") {
                Some(Value::Object(err)) => Err(ResponseError {
                    code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                }),
                _ => Ok(obj.remove("result").unwrap_or(Value::Null)),
            };
            Some(Incoming::Response { id, result })
        }
        (None, None) => None,
    }
}

/// The longest header block believed. A server that sends more than this
/// without a blank line is not speaking LSP, and the buffer must not grow
/// without bound waiting for one.
const MAX_HEADER: usize = 8 * 1024;

/// Reassembles messages from whatever pieces a pipe delivers.
#[derive(Default)]
pub(crate) struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete body, if one has arrived. A header block without a
    /// usable `Content-Length` is dropped and reading carries on after it —
    /// one garbled message costs that message, not the session.
    pub(crate) fn next_frame(&mut self) -> Option<Vec<u8>> {
        loop {
            let Some(header_end) = find(&self.buf, b"\r\n\r\n") else {
                if self.buf.len() > MAX_HEADER {
                    // No header terminator in sight: keep only a tail that
                    // could still be the start of one.
                    let keep = self.buf.len() - 3;
                    self.buf.drain(..keep);
                }
                return None;
            };
            let header = String::from_utf8_lossy(&self.buf[..header_end]);
            let length = header.split("\r\n").find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            });
            let body_start = header_end + 4;
            let Some(length) = length else {
                log::warn!("lsp: dropping a message without Content-Length: {header:?}");
                self.buf.drain(..body_start);
                continue;
            };
            if self.buf.len() < body_start + length {
                return None;
            }
            let body = self.buf[body_start..body_start + length].to_vec();
            self.buf.drain(..body_start + length);
            return Some(body);
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_round_trips_through_the_framing() {
        let msg = request(7, "textDocument/hover", json!({ "x": "é🎉" }));
        let bytes = encode(&msg);
        let mut reader = FrameReader::default();
        reader.push(&bytes);
        let body = reader.next_frame().expect("a whole frame");
        assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), msg);
        assert!(reader.next_frame().is_none());
    }

    #[test]
    fn the_length_counts_bytes_not_characters() {
        let bytes = encode(&json!("é🎉"));
        let text = String::from_utf8(bytes).unwrap();
        // `"é🎉"` is 2 quotes + 2 + 4 bytes.
        assert!(text.starts_with("Content-Length: 8\r\n\r\n"), "{text:?}");
    }

    #[test]
    fn frames_split_at_every_byte_still_come_out_whole() {
        let a = notification("a", json!({ "n": 1 }));
        let b = notification("b", json!({ "text": "π".repeat(50) }));
        let mut stream = encode(&a);
        stream.extend(encode(&b));
        let mut reader = FrameReader::default();
        let mut got = Vec::new();
        for byte in &stream {
            reader.push(std::slice::from_ref(byte));
            while let Some(body) = reader.next_frame() {
                got.push(serde_json::from_slice::<Value>(&body).unwrap());
            }
        }
        assert_eq!(got, vec![a, b]);
    }

    #[test]
    fn two_frames_in_one_read_both_come_out() {
        let mut stream = encode(&json!(1));
        stream.extend(encode(&json!(2)));
        let mut reader = FrameReader::default();
        reader.push(&stream);
        assert_eq!(reader.next_frame().as_deref(), Some(&b"1"[..]));
        assert_eq!(reader.next_frame().as_deref(), Some(&b"2"[..]));
        assert_eq!(reader.next_frame(), None);
    }

    #[test]
    fn extra_headers_and_odd_case_are_accepted() {
        let body = br#"{"jsonrpc":"2.0","method":"x"}"#;
        let mut stream = format!(
            "content-type: application/vscode-jsonrpc; charset=utf-8\r\nCONTENT-LENGTH:{}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        stream.extend_from_slice(body);
        let mut reader = FrameReader::default();
        reader.push(&stream);
        assert_eq!(reader.next_frame().as_deref(), Some(&body[..]));
    }

    #[test]
    fn a_header_without_a_length_is_skipped() {
        let mut stream = b"X-Nothing: 1\r\n\r\n".to_vec();
        stream.extend(encode(&json!(3)));
        let mut reader = FrameReader::default();
        reader.push(&stream);
        assert_eq!(reader.next_frame().as_deref(), Some(&b"3"[..]));
    }

    #[test]
    fn messages_are_sorted_by_what_they_ask() {
        assert_eq!(
            classify(br#"{"jsonrpc":"2.0","id":3,"result":{"a":1}}"#),
            Some(Incoming::Response {
                id: 3,
                result: Ok(json!({ "a": 1 }))
            })
        );
        assert_eq!(
            classify(br#"{"jsonrpc":"2.0","id":4,"error":{"code":-32601,"message":"nope"}}"#),
            Some(Incoming::Response {
                id: 4,
                result: Err(ResponseError {
                    code: -32601,
                    message: "nope".into()
                })
            })
        );
        assert_eq!(
            classify(
                br#"{"jsonrpc":"2.0","id":"abc","method":"workspace/configuration","params":{}}"#
            ),
            Some(Incoming::Request {
                id: json!("abc"),
                method: "workspace/configuration".into(),
                params: json!({})
            })
        );
        assert_eq!(
            classify(br#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics"}"#),
            Some(Incoming::Notification {
                method: "textDocument/publishDiagnostics".into(),
                params: Value::Null
            })
        );
        // A `null` result is still a result.
        assert_eq!(
            classify(br#"{"jsonrpc":"2.0","id":5,"result":null}"#),
            Some(Incoming::Response {
                id: 5,
                result: Ok(Value::Null)
            })
        );
        assert_eq!(classify(b"not json"), None);
        assert_eq!(classify(br#"{"id":"x","result":1}"#), None);
    }
}
