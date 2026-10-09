//! The request log as HAR 1.2, the format Chrome DevTools, Charles and Fiddler all import.
//!
//! Only the keys DevTools' importer actually reads are emitted, and every numeric field is a real
//! number — `-1` is the spec's "unknown", which is what most of these are: the proxy sees a
//! request and a response, not a connect/DNS/SSL breakdown.
//!
//! Bodies are streamed from their private capture files. Export memory stays bounded even when
//! the session contains hundreds of megabytes of payloads.

use crate::proxy::state::{EventKind, RequestEvent};
use crate::proxy::storage::BodySnapshot;
use serde::Serialize;
use std::io::{self, Read, Write};

#[derive(Serialize)]
pub struct Har {
    pub log: Log,
}

#[derive(Serialize)]
pub struct Log {
    pub version: &'static str,
    pub creator: Creator,
    pub entries: Vec<Entry>,
}

#[derive(Serialize)]
pub struct Creator {
    pub name: &'static str,
    pub version: &'static str,
}

#[derive(Serialize)]
pub struct Entry {
    #[serde(rename = "startedDateTime")]
    pub started_date_time: String,
    pub time: f64,
    pub request: Request,
    pub response: Response,
    pub cache: Cache,
    pub timings: Timings,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Serialize)]
pub struct Request {
    pub method: String,
    pub url: String,
    #[serde(rename = "httpVersion")]
    pub http_version: &'static str,
    pub cookies: Vec<()>,
    pub headers: Vec<Header>,
    #[serde(rename = "_trailers", skip_serializing_if = "Vec::is_empty")]
    pub trailers: Vec<Header>,
    #[serde(rename = "queryString")]
    pub query_string: Vec<Header>,
    #[serde(rename = "headersSize")]
    pub headers_size: i64,
    #[serde(rename = "bodySize")]
    pub body_size: i64,
    #[serde(rename = "postData", skip_serializing_if = "Option::is_none")]
    pub post_data: Option<PostData>,
}

#[derive(Serialize)]
pub struct Response {
    pub status: u16,
    #[serde(rename = "statusText")]
    pub status_text: String,
    #[serde(rename = "httpVersion")]
    pub http_version: &'static str,
    pub cookies: Vec<()>,
    pub headers: Vec<Header>,
    #[serde(rename = "_trailers", skip_serializing_if = "Vec::is_empty")]
    pub trailers: Vec<Header>,
    pub content: Content,
    #[serde(rename = "redirectURL")]
    pub redirect_url: String,
    #[serde(rename = "headersSize")]
    pub headers_size: i64,
    #[serde(rename = "bodySize")]
    pub body_size: i64,
}

#[derive(Serialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

#[derive(Serialize)]
pub struct Content {
    pub size: i64,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<BodyText>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<&'static str>,
}

#[derive(Serialize)]
pub struct PostData {
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    pub text: BodyText,
    #[serde(rename = "_encoding", skip_serializing_if = "Option::is_none")]
    pub encoding: Option<&'static str>,
}

#[derive(Serialize)]
pub struct Cache {}

#[derive(Serialize)]
pub struct Timings {
    pub blocked: f64,
    pub dns: f64,
    pub connect: f64,
    pub send: f64,
    pub wait: f64,
    pub receive: f64,
    pub ssl: f64,
}

#[cfg(test)]
pub fn build(events: &[RequestEvent]) -> Har {
    Har {
        log: Log {
            version: "1.2",
            creator: Creator {
                name: "Privaxy for Android",
                version: env!("CARGO_PKG_VERSION"),
            },
            // Oldest first: the log is newest-first, and a waterfall reads forwards.
            entries: events
                .iter()
                .rev()
                .map(|event| entry(event).unwrap())
                .collect(),
        },
    }
}

pub fn write(events: &[RequestEvent], mut output: impl Write) -> io::Result<()> {
    output.write_all(b"{\"log\":{\"version\":\"1.2\",\"creator\":")?;
    serde_json::to_writer(
        &mut output,
        &Creator {
            name: "Privaxy for Android",
            version: env!("CARGO_PKG_VERSION"),
        },
    )?;
    output.write_all(b",\"entries\":[")?;
    for (index, event) in events.iter().rev().enumerate() {
        if index > 0 {
            output.write_all(b",")?;
        }
        serde_json::to_writer(&mut output, &entry(event)?)?;
    }
    output.write_all(b"]}}")
}

fn entry(event: &RequestEvent) -> io::Result<Entry> {
    // Never hold the exchange lock during disk I/O or serialization.
    let exchange = event.exchange.lock().ok().map(|open| open.clone());
    let exchange = exchange.as_ref();

    let millis = exchange
        .and_then(|exchange| exchange.finished_at)
        .map(|finished| (finished - event.at).num_milliseconds() as f64)
        .unwrap_or(0.0)
        .max(0.0);

    let request_headers = exchange.map(|e| headers(&e.request_headers)).unwrap_or_default();
    let response_headers = exchange.map(|e| headers(&e.response_headers)).unwrap_or_default();
    let mime = exchange
        .and_then(|e| header_value(&e.response_headers, "content-type"))
        .unwrap_or_default();

    // A blocked or tunnelled entry has no status. Chrome writes 0 for a request that produced no
    // response, so the importer is happy with it and the waterfall still renders.
    let status = exchange.and_then(|e| e.status).unwrap_or(0);
    let status_text = if let Some(error) = exchange.and_then(|e| e.error.as_ref()) {
        error.clone()
    } else {
        match &event.kind {
            EventKind::Blocked { filter } => format!("Blocked by Privaxy ({filter})"),
            EventKind::Tunneled if status == 0 => String::from("Tunneled, not inspected"),
            EventKind::Intercepted if status == 0 => {
                String::from("TLS connection opened; requests inside are separate entries")
            }
            _ => String::new(),
        }
    };

    let response_body = exchange.map(|e| e.response_body.snapshot());
    let request_body = exchange.map(|e| e.request_body.snapshot());

    Ok(Entry {
        started_date_time: event
            .at
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
        time: millis,
        request: Request {
            method: event.method.clone(),
            url: event.url.clone(),
            http_version: version(exchange.and_then(|e| e.request_version)),
            cookies: Vec::new(),
            headers: request_headers,
            trailers: exchange
                .map(|e| headers(&e.request_trailers))
                .unwrap_or_default(),
            query_string: query_string(&event.url),
            headers_size: -1,
            body_size: request_body.as_ref().map(|b| b.seen as i64).unwrap_or(-1),
            post_data: match request_body.as_ref().filter(|b| b.len > 0) {
                Some(body) => {
                    let text = BodyText::new(body.clone())?;
                    Some(PostData {
                        mime_type: header_value(
                            exchange
                                .map(|e| e.request_headers.as_slice())
                                .unwrap_or(&[]),
                            "content-type",
                        )
                        .unwrap_or_else(|| "application/octet-stream".into()),
                        encoding: text.binary.then_some("base64"),
                        text,
                    })
                }
                None => None,
            },
        },
        response: Response {
            status,
            status_text,
            http_version: version(exchange.and_then(|e| e.response_version)),
            cookies: Vec::new(),
            headers: response_headers,
            trailers: exchange
                .map(|e| headers(&e.response_trailers))
                .unwrap_or_default(),
            content: content(response_body.as_ref(), mime)?,
            redirect_url: exchange
                .and_then(|e| header_value(&e.response_headers, "location"))
                .unwrap_or_default(),
            headers_size: -1,
            body_size: response_body.as_ref().map(|b| b.seen as i64).unwrap_or(-1),
        },
        cache: Cache {},
        // The proxy measures one span: request in, last response byte out. Everything else is
        // honestly unknown, and -1 is how HAR says so.
        timings: Timings {
            blocked: -1.0,
            dns: -1.0,
            connect: -1.0,
            send: 0.0,
            wait: millis,
            receive: 0.0,
            ssl: -1.0,
        },
        comment: {
            let notes: Vec<String> = [
                ("request", request_body.as_ref()),
                ("response", response_body.as_ref()),
            ]
            .into_iter()
            .filter_map(|(side, body)| {
                body.filter(|b| b.len != b.seen || b.error.is_some())
                    .map(|b| {
                        format!(
                            "{side} body incomplete: {} of {} bytes stored. {}",
                            b.len,
                            b.seen,
                            b.error
                                .as_deref()
                                .unwrap_or("Capture was still in progress.")
                        )
                    })
            })
            .collect();
            (!notes.is_empty()).then(|| notes.join(" "))
        },
    })
}

fn version(version: Option<http::Version>) -> &'static str {
    match version {
        Some(http::Version::HTTP_09) => "HTTP/0.9",
        Some(http::Version::HTTP_10) => "HTTP/1.0",
        Some(http::Version::HTTP_11) => "HTTP/1.1",
        Some(http::Version::HTTP_2) => "HTTP/2",
        Some(http::Version::HTTP_3) => "HTTP/3",
        _ => "",
    }
}

fn content(body: Option<&BodySnapshot>, mime: String) -> io::Result<Content> {
    let text = body
        .filter(|body| body.len > 0)
        .map(|body| BodyText::new(body.clone()))
        .transpose()?;
    Ok(Content {
        size: body.map_or(0, |b| b.seen as i64),
        mime_type: mime,
        encoding: text.as_ref().filter(|text| text.binary).map(|_| "base64"),
        text,
    })
}

/// `serde_json::Serializer::collect_str` escapes incrementally, without a body-sized String.
/// I/O failures are carried separately: returning fmt::Error for a source read would panic in
/// serde_json's Display adapter, which assumes fmt::Error means its output writer failed.
pub struct BodyText {
    snapshot: BodySnapshot,
    binary: bool,
}

impl BodyText {
    fn new(snapshot: BodySnapshot) -> io::Result<Self> {
        let binary = match utf8_chunks(snapshot.reader()?, |_| Ok(())) {
            Ok(()) => false,
            Err(error) if error.kind() == io::ErrorKind::InvalidData => true,
            Err(error) => return Err(error),
        };
        Ok(Self { snapshot, binary })
    }
}

impl Serialize for BodyText {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        struct DisplayBody<'a> {
            text: &'a BodyText,
            error: std::cell::RefCell<Option<io::Error>>,
        }
        impl std::fmt::Display for DisplayBody<'_> {
            fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let result = (|| -> io::Result<()> {
                    let reader = self.text.snapshot.reader()?;
                    if self.text.binary {
                        // Multiple of three: only the final base64 block may contain padding.
                        let mut reader = std::io::BufReader::new(reader);
                        let mut buffer = [0; 3 * 8192];
                        loop {
                            let mut count = 0;
                            while count < buffer.len() {
                                let read = reader.read(&mut buffer[count..])?;
                                if read == 0 {
                                    break;
                                }
                                count += read;
                            }
                            if count == 0 {
                                break;
                            }
                            output
                                .write_str(&encode_base64(&buffer[..count]))
                                .map_err(io::Error::other)?;
                        }
                        Ok(())
                    } else {
                        utf8_chunks(reader, |text| {
                            output.write_str(text).map_err(io::Error::other)
                        })
                    }
                })();
                if let Err(error) = result {
                    *self.error.borrow_mut() = Some(error);
                }
                Ok(())
            }
        }
        let display = DisplayBody {
            text: self,
            error: Default::default(),
        };
        let result = serializer.collect_str(&display);
        if let Some(error) = display.error.into_inner() {
            return Err(serde::ser::Error::custom(error));
        }
        result
    }
}

fn utf8_chunks(
    mut reader: impl Read,
    mut emit: impl FnMut(&str) -> io::Result<()>,
) -> io::Result<()> {
    let mut buffer = [0; 32768];
    let mut carry = 0;
    loop {
        let read = reader.read(&mut buffer[carry..])?;
        let len = carry + read;
        if len == 0 {
            return Ok(());
        }
        let valid = match std::str::from_utf8(&buffer[..len]) {
            Ok(text) => {
                emit(text)?;
                len
            }
            Err(error) if error.error_len().is_none() && read > 0 => {
                let valid = error.valid_up_to();
                emit(std::str::from_utf8(&buffer[..valid]).unwrap())?;
                valid
            }
            Err(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, "Not UTF-8")),
        };
        carry = len - valid;
        buffer.copy_within(valid..len, 0);
        if read == 0 {
            return Ok(());
        }
    }
}

fn headers(pairs: &[(String, String)]) -> Vec<Header> {
    pairs
        .iter()
        .map(|(name, value)| Header {
            name: name.clone(),
            value: value.clone(),
        })
        .collect()
}

fn header_value(pairs: &[(String, String)], name: &str) -> Option<String> {
    pairs
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

fn query_string(url: &str) -> Vec<Header> {
    let Some((_, query)) = url.split_once('?') else {
        return Vec::new();
    };
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            Header {
                name: name.to_owned(),
                value: value.to_owned(),
            }
        })
        .collect()
}

fn encode_base64(input: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let block = (u32::from(chunk[0]) << 16)
            | (chunk.get(1).map_or(0, |b| u32::from(*b)) << 8)
            | chunk.get(2).map_or(0, |b| u32::from(*b));
        for index in 0..4 {
            if index <= chunk.len() {
                let shift = 18 - index * 6;
                out.push(ALPHABET[((block >> shift) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::config::MitmMode;
    use crate::proxy::state::{EventKind, ProxyState, RequestEvent};

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(encode_base64(b"foob"), "Zm9vYg==");
        assert_eq!(encode_base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode_base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn splits_the_query_string() {
        let query = query_string("https://a.test/x?b=1&c=&d");
        assert_eq!(query.len(), 3);
        assert_eq!((query[0].name.as_str(), query[0].value.as_str()), ("b", "1"));
        assert_eq!((query[1].name.as_str(), query[1].value.as_str()), ("c", ""));
        assert_eq!((query[2].name.as_str(), query[2].value.as_str()), ("d", ""));
        assert!(query_string("https://a.test/x").is_empty());
    }

    /// Every key DevTools' importer dereferences must be present with the right JSON type.
    #[test]
    fn exports_entire_large_unicode_and_binary_bodies_after_many_requests() {
        let state = ProxyState::new(MitmMode::Full);
        let text = format!("{}END-OF-FULL-RESPONSE", "é\"\n".repeat(150_000));
        let first = state.record(RequestEvent::now(
            "POST",
            "https://example.com/large",
            EventKind::Proxied,
        ));
        first.lock().unwrap().record_response_chunk(text.as_bytes());
        let binary = (0..300_000).map(|i| (i % 256) as u8).collect::<Vec<_>>();
        first.lock().unwrap().record_request_chunk(&binary);
        for _ in 0..800 {
            state.record(RequestEvent::now(
                "GET",
                "https://example.com/new",
                EventKind::Proxied,
            ));
        }
        let mut output = Vec::new();
        write(&state.recent_events(usize::MAX), &mut output).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&output).unwrap();
        let entry = &json["log"]["entries"][0];
        assert_eq!(entry["response"]["content"]["text"], text);
        assert_eq!(entry["request"]["postData"]["text"], encode_base64(&binary));
        assert_eq!(entry["request"]["postData"]["_encoding"], "base64");
        assert!(entry.get("comment").is_none());
    }

    #[test]
    fn export_propagates_output_errors() {
        struct Broken;
        impl std::io::Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("disk full"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert!(write(&[], Broken).is_err());
    }

    #[test]
    fn emits_the_keys_devtools_requires() {
        let state = ProxyState::new(MitmMode::Full);
        let exchange = state.record(RequestEvent::now(
            "GET",
            "https://example.com/a?b=1",
            EventKind::Proxied,
        ));
        {
            let mut open = exchange.lock().unwrap();
            open.status = Some(200);
            open.request_headers = vec![(String::from("host"), String::from("example.com"))];
            open.response_headers =
                vec![(String::from("content-type"), String::from("text/html"))];
            open.record_response_chunk(b"<html></html>");
        }

        let value = serde_json::to_value(build(&state.recent_events(10))).unwrap();
        let log = &value["log"];
        assert_eq!(log["version"], "1.2");
        assert!(log["creator"]["name"].is_string());
        let entry = &log["entries"][0];
        assert!(entry["startedDateTime"].is_string());
        assert!(entry["time"].is_number());
        assert!(entry["cache"].is_object());
        for key in ["blocked", "dns", "connect", "send", "wait", "receive", "ssl"] {
            assert!(entry["timings"][key].is_number(), "timings.{key}");
        }
        for key in ["headersSize", "bodySize"] {
            assert!(entry["request"][key].is_number(), "request.{key}");
            assert!(entry["response"][key].is_number(), "response.{key}");
        }
        assert!(entry["response"]["status"].is_number());
        assert!(entry["response"]["content"]["size"].is_number());
        assert_eq!(entry["response"]["content"]["text"], "<html></html>");
        assert_eq!(entry["request"]["queryString"][0]["name"], "b");
    }

    #[test]
    fn a_blocked_entry_still_produces_a_valid_response_object() {
        let state = ProxyState::new(MitmMode::HostnameOnly);
        state.record(RequestEvent::now(
            "CONNECT",
            "https://ads.example.com/",
            EventKind::Blocked {
                filter: String::from("||ads.example.com^"),
            },
        ));

        let value = serde_json::to_value(build(&state.recent_events(10))).unwrap();
        let response = &value["log"]["entries"][0]["response"];
        assert_eq!(response["status"], 0);
        assert!(
            response["statusText"]
                .as_str()
                .unwrap()
                .contains("||ads.example.com^")
        );
        assert!(response["content"]["size"].is_number());
    }

    #[test]
    fn binary_bodies_are_base64_with_the_encoding_flag() {
        let state = ProxyState::new(MitmMode::Full);
        let exchange = state.record(RequestEvent::now(
            "GET",
            "https://example.com/i.png",
            EventKind::Proxied,
        ));
        exchange
            .lock()
            .unwrap()
            .record_response_chunk(&[0x89, b'P', b'N', b'G', 0xff, 0xfe]);

        let value = serde_json::to_value(build(&state.recent_events(10))).unwrap();
        let content = &value["log"]["entries"][0]["response"]["content"];
        assert_eq!(content["encoding"], "base64");
        assert_eq!(content["text"], encode_base64(&[0x89, b'P', b'N', b'G', 0xff, 0xfe]));
    }
}
