//! Per-connection request handling: CONNECT interception, tunneling, and the filtered forward path.

use crate::proxy::blocker::{BlockRule, FilterEngine};
use crate::proxy::body::CaptureBody;
use crate::proxy::cert::CertCache;
use crate::proxy::config::MitmMode;
use crate::proxy::exclusions::ExclusionStore;
use crate::proxy::state::{EventKind, Exchange, Origin, ProxyState, RequestEvent};
use bytes::Bytes;
use futures::StreamExt;
use http::uri::{Authority, Scheme};
use http::{HeaderMap, Uri, header};
use http_body_util::{BodyExt, BodyStream, Empty, Full, StreamBody, combinators::BoxBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::{http1, http2};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::collections::{HashSet, VecDeque};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;

pub type ProxyBody = BoxBody<Bytes, std::io::Error>;

const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const TUNNEL_TIMEOUT: Duration = Duration::from_secs(600);
// HTML past this still streams, but is no longer read for ids and classes.
const MAX_PARSED_HTML: usize = 8 * 1024 * 1024;
// lol_html buffers a tag split across reads; a huge data: URI attribute would otherwise sit there.
const MAX_PARSER_MEMORY: usize = 4 * 1024 * 1024;

pub const HOP_BY_HOP_HEADERS: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

pub struct Session {
    pub engine: Arc<FilterEngine>,
    pub client: reqwest::Client,
    pub certs: CertCache,
    pub exclusions: ExclusionStore,
    pub intercepts: ExclusionStore,
    pub state: Arc<ProxyState>,
    pub tls_client_config: Arc<rustls::ClientConfig>,
    /// TCP goes to the paired desktop, which owns filtering; the relay blocks nothing locally.
    pub desktop_routing: bool,
}

impl Session {
    /// Hosts whose TLS this session terminates when they arrive on an HTTPS port.
    pub fn inspects(&self, host: &str) -> bool {
        !self.exclusions.contains(host)
            && (self.state.mode() == MitmMode::Full || self.intercepts.contains(host))
    }

    /// A DNS answer to withhold. Inspected hosts are left to URL rules so blocks keep their detail.
    pub fn dns_block(&self, host: &str) -> Option<BlockRule> {
        if self.desktop_routing || self.inspects(host) {
            return None;
        }
        self.engine.check_host(host)
    }

    /// QUIC to refuse; the app retries over TCP, where the proxy decides again.
    pub fn quic_block(&self, host: &str) -> Option<BlockRule> {
        if self.desktop_routing {
            return None;
        }
        self.engine.check_host(host)
    }
}

pub async fn handle(
    session: Arc<Session>,
    request: Request<Incoming>,
    origin: Option<Arc<Origin>>,
) -> Result<Response<ProxyBody>, Infallible> {
    let Some(authority) = target_authority(&request) else {
        return Ok(message_response(
            StatusCode::BAD_REQUEST,
            "Privaxy could not determine which host this request was for.",
        ));
    };

    if request.method() == Method::CONNECT {
        Ok(handle_connect(session, request, authority, origin))
    } else {
        Ok(forward(session, request, authority, Scheme::HTTP, origin).await)
    }
}

/// CONNECT carries only `host:port`, so this is where hostname-level blocking happens — and it is
/// the only filtering that works against apps which do not trust the local CA.
fn handle_connect(
    session: Arc<Session>,
    request: Request<Incoming>,
    authority: Authority,
    origin: Option<Arc<Origin>>,
) -> Response<ProxyBody> {
    let host = authority.host().to_owned();
    let logged_url = format!("https://{authority}/");

    // Never-intercept always wins: it is the safety valve for pinned apps, so a host on both
    // lists stays tunnelled. Otherwise the intercept list terminates a host that hostname-only
    // mode would have passed through — picking a few hosts rather than switching the whole device
    // to Full inspection, which breaks every app that does not trust a user CA.
    let excluded = session.exclusions.contains(&host);
    let intercepted = !excluded && session.intercepts.contains(&host);
    let other_protocol = !crate::vpn::sniff::TLS_PORTS
        .contains(&authority.port_u16().unwrap_or(443))
        && !intercepted;
    let tunnel_only = excluded
        || other_protocol
        || (session.state.mode() == MitmMode::HostnameOnly && !intercepted);

    // Inspected connections must negotiate TLS first: apply URL rules to the inner
    // request, where path, headers and content type are available.
    if tunnel_only && let Some(rule) = session.engine.check_host(&host) {
        session.state.record(
            RequestEvent::now("CONNECT", &logged_url, EventKind::Blocked { rule })
                .with_origin(origin)
                .note(
                    "Refused as the tunnel opened, on the hostname alone. Nothing was sent, so \
                     there are no headers or body to show.",
                ),
        );
        // Refusing the tunnel surfaces as a failed connection in the client, which is what an
        // ad request should look like.
        return message_response(StatusCode::FORBIDDEN, "Blocked by Privaxy.");
    }

    // Every HTTPS connection produces one of these rows, so labelling matters: an intercepted
    // CONNECT used to be recorded as "tunneled" with a note saying TLS was *not* terminated —
    // the opposite of what happens — which is why the log reads as nothing but tunnels.
    let (kind, note) = if excluded {
        (
            EventKind::Tunneled,
            format!(
                "{host} is on the never-intercept list, so this connection is passed through byte \
                 for byte. Only the hostname is visible."
            ),
        )
    } else if other_protocol {
        (
            EventKind::Tunneled,
            String::from(
                "This port is not a standard HTTPS port (443 or 8443). Its protocol is passed through \
             unchanged. If it serves HTTPS, add the host to Inspect these hosts to inspect it.",
            ),
        )
    } else if tunnel_only {
        (
            EventKind::Tunneled,
            String::from(
                "TLS was tunnelled without being terminated, so only the hostname is visible. \
                 Add this host to Inspect these hosts — and install the certificate — to see \
                 inside it without switching the whole device to Full inspection.",
            ),
        )
    } else {
        (
            EventKind::Intercepted,
            String::from("Opening the connection; the requests inside are logged separately."),
        )
    };

    let exchange = session.state.record(
        RequestEvent::now("CONNECT", &logged_url, kind)
            .with_origin(origin.clone())
            .note(note),
    );

    tokio::spawn(async move {
        let upgraded = match hyper::upgrade::on(request).await {
            Ok(upgraded) => TokioIo::new(upgraded),
            Err(error) => {
                log::debug!("CONNECT upgrade failed for {host}: {error}");
                note_failure(&exchange, &format!("CONNECT upgrade failed: {error}"));
                return;
            }
        };

        if tunnel_only {
            match tokio::time::timeout(TUNNEL_TIMEOUT, tunnel(upgraded, &authority)).await {
                Ok(Ok(())) => {
                    if let Ok(mut open) = exchange.lock() {
                        open.finished_at = Some(chrono::Local::now());
                    }
                }
                Ok(Err(error)) => note_failure(&exchange, &format!("Tunnel failed: {error}")),
                Err(_) => note_failure(&exchange, "Tunnel reached its 10-minute connection limit."),
            }
        } else {
            if tokio::time::timeout(
                TUNNEL_TIMEOUT,
                intercept_tls(session, upgraded, authority, exchange.clone(), origin),
            )
            .await
            .is_err()
            {
                note_failure(
                    &exchange,
                    "Inspected connection reached its 10-minute connection limit.",
                );
            }
        }
    });

    Response::new(empty_body())
}

/// Copies bytes between client and origin without looking at them.
async fn tunnel<T>(mut client: T, authority: &Authority) -> std::io::Result<()>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let port = authority.port_u16().unwrap_or(443);
    let mut origin = TcpStream::connect((unbracket(authority.host()), port)).await?;
    tokio::io::copy_bidirectional(&mut client, &mut origin).await?;
    Ok(())
}

/// Terminates TLS with a certificate minted for this host, then filters the requests inside.
async fn intercept_tls<T>(
    session: Arc<Session>,
    client: T,
    authority: Authority,
    exchange: Arc<Mutex<Exchange>>,
    origin: Option<Arc<Origin>>,
) where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // The CONNECT row reports the negotiation separately from the HTTP requests inside.
    let note = |text: String| {
        if let Ok(mut exchange) = exchange.lock() {
            exchange.note = Some(text);
        }
    };

    let server_config = match session.certs.server_config(&authority).await {
        Ok(config) => config,
        Err(error) => {
            log::warn!("No certificate for {authority}: {error}");
            note_failure(
                &exchange,
                &format!("Could not mint a certificate for this host: {error}"),
            );
            return;
        }
    };

    let tls_stream = match tokio::time::timeout(
        TLS_HANDSHAKE_TIMEOUT,
        TlsAcceptor::from(server_config).accept(client),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            log::debug!("TLS handshake with {authority} failed: {error}");
            note_failure(&exchange, &tls_failure(&error));
            return;
        }
        Err(_) => {
            log::debug!("TLS handshake with {authority} timed out");
            note_failure(
                &exchange,
                "The TLS handshake timed out before an HTTP request arrived.",
            );
            return;
        }
    };

    let http2 = tls_stream.get_ref().1.alpn_protocol() == Some(b"h2");
    note(format!(
        "TLS established using {}. Requests inside this connection appear as separate rows.",
        if http2 { "HTTP/2" } else { "HTTP/1.1" }
    ));

    let service = service_fn(move |request| {
        let session = session.clone();
        let authority = authority.clone();
        let origin = origin.clone();
        async move {
            Ok::<_, Infallible>(forward(session, request, authority, Scheme::HTTPS, origin).await)
        }
    });

    let result = if http2 {
        http2::Builder::new(TokioExecutor::new())
            .serve_connection(TokioIo::new(tls_stream), service)
            .await
    } else {
        http1::Builder::new()
            .preserve_header_case(true)
            .title_case_headers(true)
            .serve_connection(TokioIo::new(tls_stream), service)
            .with_upgrades()
            .await
    };
    if let Err(error) = result {
        note_failure(
            &exchange,
            &format!("TLS succeeded, but the HTTP connection failed: {error}"),
        );
    } else if let Ok(mut open) = exchange.lock() {
        open.finished_at = Some(chrono::Local::now());
    }
}

fn tls_failure(error: &std::io::Error) -> String {
    use rustls::{AlertDescription, Error};
    match error
        .get_ref()
        .and_then(|source| source.downcast_ref::<Error>())
    {
        Some(Error::AlertReceived(
            AlertDescription::UnknownCA
            | AlertDescription::BadCertificate
            | AlertDescription::CertificateUnknown
            | AlertDescription::CertificateExpired
            | AlertDescription::CertificateRevoked
            | AlertDescription::UnsupportedCertificate,
        )) => format!(
            "The client rejected the TLS certificate. An installed user CA is not automatically \
             trusted by apps targeting Android 7+. The app may use its own trust store or \
             certificate pinning. Check the CA fingerprint in Settings; use Never intercept \
             for apps that must connect without inspection. ({error})"
        ),
        Some(
            Error::NoApplicationProtocol
            | Error::AlertReceived(AlertDescription::NoApplicationProtocol),
        ) => format!(
            "The client could not negotiate HTTP/2 or HTTP/1.1. This may be a non-HTTP TLS protocol; use Never intercept for this host. ({error})"
        ),
        _ => format!(
            "TLS negotiation failed before an HTTP request arrived. This alone does not prove certificate rejection; the client may have closed the connection or used an unsupported protocol. ({error})"
        ),
    }
}

fn unbracket(host: &str) -> &str {
    host.trim_start_matches('[').trim_end_matches(']')
}

async fn forward(
    session: Arc<Session>,
    request: Request<Incoming>,
    authority: Authority,
    scheme: Scheme,
    origin: Option<Arc<Origin>>,
) -> Response<ProxyBody> {
    let Ok(uri) = Uri::builder()
        .scheme(scheme.clone())
        .authority(authority.clone())
        .path_and_query(
            request
                .uri()
                .path_and_query()
                .map(|path| path.as_str())
                .unwrap_or("/"),
        )
        .build()
    else {
        return message_response(StatusCode::BAD_REQUEST, "Malformed request URL.");
    };

    let url = uri.to_string();
    let referer = request
        .headers()
        .get(header::REFERER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        // Without a referer the engine treats everything as third party and over-blocks.
        .unwrap_or_else(|| url.clone());

    if let Some(rule) = session.engine.check(
        &url,
        &referer,
        request_type(request.headers()),
        request.method().as_str(),
    ) {
        let exchange = session.state.record(
            RequestEvent::now(request.method().as_str(), &url, EventKind::Blocked { rule })
                .with_origin(origin)
                .note(
                    "Blocked before the request left the phone, so there is no response to show.",
                ),
        );
        if let Ok(mut exchange) = exchange.lock() {
            exchange.status = Some(403);
            exchange.request_headers = header_pairs(request.headers());
            exchange.request_version = Some(request.version());
            exchange.finished_at = Some(chrono::Local::now());
        }
        return message_response(StatusCode::FORBIDDEN, "Blocked by Privaxy.");
    }

    if request.headers().contains_key(header::UPGRADE) {
        return upgrade_through(session, request, uri, scheme, origin).await;
    }

    let exchange = session.state.record(
        RequestEvent::now(request.method().as_str(), &url, EventKind::Proxied).with_origin(origin),
    );
    if let Ok(mut open) = exchange.lock() {
        // What the client actually sent, before hop-by-hop headers are stripped for the origin.
        open.request_headers = header_pairs(request.headers());
        open.request_version = Some(request.version());
    }

    let method = request.method().clone();
    let is_head = method == Method::HEAD;
    let mut headers = request.headers().clone();
    strip_hop_by_hop(&mut headers);
    headers.remove(header::HOST);

    // Streamed rather than collected so a large upload is not held in memory; the tee keeps a
    // disk-backed copy on the way past.
    let body = reqwest::Body::wrap(CaptureBody::new(
        request.into_body(),
        exchange.clone(),
        true,
    ));

    let response = match session
        .client
        .request(method, &url)
        .headers(headers)
        .body(body)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            // The exchange is already logged, so without this the row shows no status and no
            // reason — a DNS failure and a refused connection look identical to an empty response.
            let message = format!(
                "Privaxy could not reach {}: {}",
                authority.host(),
                error_chain(&error)
            );
            note_failure(&exchange, &message);
            return message_response(StatusCode::BAD_GATEWAY, &message);
        }
    };

    let status = response.status();
    let mut response_headers = response.headers().clone();
    if let Ok(mut open) = exchange.lock() {
        open.status = Some(status.as_u16());
        open.response_version = Some(response.version());
        // reqwest removes encoding/length itself when it decompresses a supported encoding.
        open.response_headers = header_pairs(&response_headers);
    }
    strip_hop_by_hop(&mut response_headers);
    // The proxy cannot inspect advertised QUIC alternatives. Do not encourage a captured
    // client to leave this working TCP connection for UDP (or retry the VPN's QUIC drop).
    if session.engine.is_enabled() {
        response_headers.remove(header::ALT_SVC);
    }
    // Preserve unsupported encodings (e.g. zstd) instead of mislabelling compressed bytes as
    // plaintext. Never rewrite encoded bodies, HEADs or byte ranges.
    let is_html = session.engine.is_enabled()
        && !is_head
        && status != StatusCode::PARTIAL_CONTENT
        && !response_headers.contains_key(header::CONTENT_ENCODING)
        && response_headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/html"));

    let body = if is_html {
        response_headers.remove(header::CONTENT_LENGTH);
        cosmetic_html_body(response, url, session.clone(), exchange.clone())
    } else {
        stream_body(response, exchange.clone())
    };

    let mut proxied = Response::new(body);
    *proxied.status_mut() = status;
    *proxied.headers_mut() = response_headers;
    proxied
}

/// Marks a logged exchange as finished with the reason it produced no response.
fn note_failure(exchange: &Arc<std::sync::Mutex<Exchange>>, message: &str) {
    if let Ok(mut open) = exchange.lock() {
        open.fail(message);
    }
}

fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        message.push_str(": ");
        message.push_str(&error.to_string());
        source = error.source();
    }
    message
}

/// Protocol upgrades (WebSocket, mostly) cannot be filtered, so both ends are joined and the
/// bytes are passed through. Without this, terminating TLS would break every socket-based app.
async fn upgrade_through(
    session: Arc<Session>,
    mut request: Request<Incoming>,
    uri: Uri,
    scheme: Scheme,
    origin: Option<Arc<Origin>>,
) -> Response<ProxyBody> {
    let host = uri.host().unwrap_or_default().to_owned();
    let port = uri
        .port_u16()
        .unwrap_or(if scheme == Scheme::HTTPS { 443 } else { 80 });

    // forward() hands upgrades over before it records, so without this a WebSocket app is not
    // partly visible in the log — it is absent. The frames themselves are opaque once joined,
    // which the note says.
    let exchange = session.state.record(
        RequestEvent::now(
            request.method().as_str(),
            &uri.to_string(),
            EventKind::Proxied,
        )
        .with_origin(origin)
        .note(
            "Protocol upgrade — the connection is joined end to end after the handshake, so the \
             frames are not logged.",
        ),
    );
    if let Ok(mut open) = exchange.lock() {
        open.request_headers = header_pairs(request.headers());
        open.request_version = Some(request.version());
    }

    let origin: Box<dyn Stream> = match connect_origin(
        &host,
        port,
        scheme == Scheme::HTTPS,
        &session.tls_client_config,
    )
    .await
    {
        Ok(stream) => stream,
        Err(error) => {
            let message =
                format!("Privaxy could not open an upgrade connection to {host}: {error}");
            note_failure(&exchange, &message);
            return message_response(StatusCode::BAD_GATEWAY, &message);
        }
    };

    let (mut sender, connection) =
        match hyper::client::conn::http1::handshake(TokioIo::new(origin)).await {
            Ok(pair) => pair,
            Err(error) => {
                note_failure(
                    &exchange,
                    &format!("Upgrade handshake with {host} failed: {error}"),
                );
                return message_response(
                    StatusCode::BAD_GATEWAY,
                    &format!("Upgrade handshake with {host} failed: {error}"),
                );
            }
        };
    tokio::spawn(connection.with_upgrades());

    let mut upstream_request = Request::builder().method(request.method().clone()).uri(
        uri.path_and_query()
            .map(|path| path.as_str())
            .unwrap_or("/"),
    );
    if let Some(headers) = upstream_request.headers_mut() {
        *headers = request.headers().clone();
    }
    let Ok(upstream_request) = upstream_request.body(Empty::<Bytes>::new()) else {
        note_failure(&exchange, "Malformed upgrade request.");
        return message_response(StatusCode::BAD_REQUEST, "Malformed upgrade request.");
    };

    let mut upstream_response = match sender.send_request(upstream_request).await {
        Ok(response) => response,
        Err(error) => {
            let message = format!("Upgrade request to {host} failed: {error}");
            note_failure(&exchange, &message);
            return message_response(StatusCode::BAD_GATEWAY, &message);
        }
    };

    let status = upstream_response.status();
    let headers = upstream_response.headers().clone();
    if let Ok(mut open) = exchange.lock() {
        open.status = Some(status.as_u16());
        open.response_version = Some(upstream_response.version());
        open.response_headers = header_pairs(&headers);
        if status == StatusCode::SWITCHING_PROTOCOLS {
            open.finished_at = Some(chrono::Local::now());
        }
    }

    if status != StatusCode::SWITCHING_PROTOCOLS {
        // A refused upgrade is an ordinary HTTP response, including its diagnostic body.
        let (mut parts, body) = upstream_response.into_parts();
        strip_hop_by_hop(&mut parts.headers);
        return Response::from_parts(
            parts,
            CaptureBody::new(body, exchange, false)
                .map_err(std::io::Error::other)
                .boxed(),
        );
    }

    if status == StatusCode::SWITCHING_PROTOCOLS {
        tokio::spawn(async move {
            let (client, origin) = tokio::join!(
                hyper::upgrade::on(&mut request),
                hyper::upgrade::on(&mut upstream_response)
            );
            match (client, origin) {
                (Ok(client), Ok(origin)) => {
                    let mut client = TokioIo::new(client);
                    let mut origin = TokioIo::new(origin);
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut origin).await;
                }
                (client, origin) => {
                    log::debug!("Upgrade join failed: {:?} {:?}", client.err(), origin.err());
                }
            }
        });
    }

    let mut response = Response::new(empty_body());
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

async fn connect_origin(
    host: &str,
    port: u16,
    tls: bool,
    tls_config: &Arc<rustls::ClientConfig>,
) -> std::io::Result<Box<dyn Stream>> {
    let stream = TcpStream::connect((unbracket(host), port)).await?;
    if !tls {
        return Ok(Box::new(stream));
    }

    let server_name = rustls_pki_types::ServerName::try_from(unbracket(host).to_owned())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let stream = tokio_rustls::TlsConnector::from(tls_config.clone())
        .connect(server_name, stream)
        .await?;
    Ok(Box::new(stream))
}

/// The authority the request targets: absolute-form URI first, then the Host header.
fn target_authority(request: &Request<Incoming>) -> Option<Authority> {
    if let Some(authority) = request.uri().authority() {
        return Some(authority.clone());
    }

    request
        .headers()
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .and_then(|host| host.parse().ok())
}

/// The filter engine weighs rules by resource type, so a guess from the request's own hints beats
/// labelling everything "other".
fn request_type(headers: &HeaderMap) -> &'static str {
    if let Some(destination) = headers
        .get("sec-fetch-dest")
        .and_then(|value| value.to_str().ok())
    {
        return match destination {
            "document" => "document",
            "iframe" | "frame" | "embed" | "object" => "subdocument",
            "script" | "worker" | "sharedworker" | "serviceworker" => "script",
            "image" => "image",
            "style" => "stylesheet",
            "font" => "font",
            "audio" | "video" | "track" => "media",
            "empty" => "xmlhttprequest",
            _ => "other",
        };
    }

    match headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
    {
        Some(accept) if accept.contains("text/html") => "document",
        Some(accept) if accept.contains("text/css") => "stylesheet",
        Some(accept) if accept.starts_with("image/") => "image",
        _ => "other",
    }
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    // Connection can nominate additional headers that belong only to this hop.
    let nominated: Vec<_> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| http::HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    let trailers = headers
        .get(header::TE)
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"trailers"));
    for name in nominated {
        headers.remove(name);
    }
    for name in HOP_BY_HOP_HEADERS {
        headers.remove(name);
    }
    // This is the single TE value HTTP/2 permits; gRPC uses it to request response trailers.
    if trailers {
        headers.insert(header::TE, header::HeaderValue::from_static("trailers"));
    }
}

/// Streams the response through to the client, saving the body for the inspector on the
/// way past. The whole body is never held: only what fits the cap, plus a running byte count.
fn stream_body(response: reqwest::Response, exchange: Arc<Mutex<Exchange>>) -> ProxyBody {
    CaptureBody::new(reqwest::Body::from(response), exchange, false)
        .map_err(std::io::Error::other)
        .boxed()
}

#[derive(Default)]
struct Found {
    ids: HashSet<String>,
    classes: HashSet<String>,
}

/// Reads ids and classes from a copy of the page; the page itself is forwarded as it arrives.
struct Collector {
    rewriter: Option<lol_html::send::HtmlRewriter<'static, fn(&[u8])>>,
    found: Arc<Mutex<Found>>,
    parsed: usize,
}

impl Collector {
    fn new() -> Self {
        use lol_html::{MemorySettings, element, send};

        let found = Arc::new(Mutex::new(Found::default()));
        let into = found.clone();
        let settings = send::Settings::new_send()
            .with_memory_settings(
                MemorySettings::new().with_max_allowed_memory_usage(MAX_PARSER_MEMORY),
            )
            .append_element_content_handler(element!("*", move |element| {
                if let Ok(mut found) = into.lock() {
                    if let Some(id) = element.get_attribute("id") {
                        found.ids.insert(id);
                    }
                    if let Some(class) = element.get_attribute("class") {
                        found.classes.extend(class.split_whitespace().map(str::to_owned));
                    }
                }
                Ok(())
            }));
        let discard: fn(&[u8]) = |_| {};
        Self {
            rewriter: Some(send::HtmlRewriter::new(settings, discard)),
            found,
            parsed: 0,
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        let Some(rewriter) = self.rewriter.as_mut() else {
            return;
        };
        self.parsed = self.parsed.saturating_add(chunk.len());
        // A parse error or the budget ends collection; what was found still counts.
        if self.parsed > MAX_PARSED_HTML || rewriter.write(chunk).is_err() {
            self.rewriter = None;
        }
    }

    /// The hiding stylesheet and scriptlets for everything collected.
    fn finish(&mut self, url: &str, session: &Session) -> Option<Bytes> {
        if let Some(rewriter) = self.rewriter.take() {
            let _ = rewriter.end();
        }
        let found = std::mem::take(&mut *self.found.lock().ok()?);
        let ids: Vec<String> = found.ids.into_iter().collect();
        let classes: Vec<String> = found.classes.into_iter().collect();
        let cosmetic = session.engine.cosmetic(url, &ids, &classes);
        if cosmetic.hidden_selectors.is_empty() && cosmetic.injected_script.is_none() {
            return None;
        }

        session.state.note_modified_response();

        let mut appended = String::new();
        if !cosmetic.hidden_selectors.is_empty() {
            appended.push_str("<style>");
            appended.push_str(&cosmetic.hidden_selectors.join(","));
            appended.push_str("{display:none !important}</style>");
        }
        if let Some(script) = cosmetic.injected_script {
            appended.push_str("<script type=\"application/javascript\">");
            appended.push_str(&script);
            appended.push_str("</script>");
        }
        Some(Bytes::from(appended))
    }
}

/// Streams HTML through unchanged, then appends the stylesheet for the ids and classes it held.
fn cosmetic_html_body(
    response: reqwest::Response,
    url: String,
    session: Arc<Session>,
    exchange: Arc<Mutex<Exchange>>,
) -> ProxyBody {
    struct State {
        upstream: BodyStream<reqwest::Body>,
        // Mutex only for Sync: the body is polled by one task at a time.
        collector: Mutex<Collector>,
        queued: VecDeque<Frame<Bytes>>,
        done: bool,
        url: String,
        session: Arc<Session>,
    }

    impl State {
        fn finish(&mut self) {
            self.done = true;
            let collector = self.collector.get_mut().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(tail) = collector.finish(&self.url, &self.session) {
                self.queued.push_back(Frame::data(tail));
            }
        }
    }

    let state = State {
        upstream: BodyStream::new(reqwest::Body::from(response)),
        collector: Mutex::new(Collector::new()),
        queued: VecDeque::new(),
        done: false,
        url,
        session,
    };
    let stream = futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(frame) = state.queued.pop_front() {
                return Some((Ok(frame), state));
            }
            if state.done {
                return None;
            }
            match state.upstream.next().await {
                Some(Ok(frame)) => {
                    if let Some(data) = frame.data_ref() {
                        state
                            .collector
                            .get_mut()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .feed(data);
                        return Some((Ok(frame), state));
                    }
                    // Trailers end the body, so the stylesheet goes ahead of them.
                    state.finish();
                    state.queued.push_back(frame);
                }
                Some(Err(error)) => {
                    state.done = true;
                    return Some((Err(error), state));
                }
                None => state.finish(),
            }
        }
    });
    CaptureBody::new(StreamBody::new(stream), exchange, false)
        .map_err(std::io::Error::other)
        .boxed()
}

pub(super) fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let value = value
                .to_str()
                .map(str::to_owned)
                // Header values are bytes; a non-ASCII one is shown rather than dropped.
                .unwrap_or_else(|_| String::from_utf8_lossy(value.as_bytes()).into_owned());
            (name.as_str().to_owned(), value)
        })
        .collect()
}

fn empty_body() -> ProxyBody {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

fn full_body(bytes: impl Into<Bytes>) -> ProxyBody {
    Full::new(bytes.into())
        .map_err(|never| match never {})
        .boxed()
}

fn message_response(status: StatusCode, message: &str) -> Response<ProxyBody> {
    let page = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>Privaxy</title></head>\
         <body style=\"font-family:system-ui,sans-serif;background:#12121a;color:#e6e6ef;\
         display:flex;align-items:center;justify-content:center;height:100vh;margin:0\">\
         <div style=\"text-align:center;padding:1.5rem\"><h1>Privaxy</h1><p>{message}</p></div>\
         </body></html>"
    );

    let mut response = Response::new(full_body(page));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
