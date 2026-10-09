//! Local two-sided TLS tests: no public server, phone, or installed user CA is required.
use super::*;
use crate::proxy::ca::CertAuthority;
use http::Version;
use tokio::net::TcpListener;

struct Harness {
    session: Arc<Session>,
    client: reqwest::Client,
    origin: String,
    received: Arc<Mutex<Exchange>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Harness {
    async fn new(h2: bool) -> Self {
        crate::proxy::install_crypto_provider();
        let ca = CertAuthority::generate().unwrap();
        let certs = CertCache::new(&ca).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(rustls_pki_types::CertificateDer::from(
                crate::proxy::ca::pem_to_der(&ca.certificate_pem).unwrap(),
            ))
            .unwrap();
        let mut tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls.alpn_protocols = vec![if h2 {
            b"h2".to_vec()
        } else {
            b"http/1.1".to_vec()
        }];
        let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let authority: Authority = format!("localhost:{}", origin.local_addr().unwrap().port())
            .parse()
            .unwrap();
        let mut server_tls = (*certs.server_config(&authority).await.unwrap()).clone();
        server_tls.alpn_protocols = tls.alpn_protocols.clone();
        let received = Arc::new(Mutex::new(Exchange::for_test()));
        let observed = received.clone();
        let origin_task = tokio::spawn(async move {
            let (stream, _) = origin.accept().await.unwrap();
            let stream = TlsAcceptor::from(Arc::new(server_tls))
                .accept(stream)
                .await
                .unwrap();
            let service = service_fn(move |request: Request<Incoming>| {
                let observed = observed.clone();
                async move {
                    let path = request.uri().path().to_owned();
                    let version = request.version();
                    let body = request.into_body().collect().await.unwrap();
                    let trailers = body.trailers().cloned().unwrap_or_default();
                    let bytes = body.to_bytes();
                    {
                        let mut open = observed.lock().unwrap();
                        open.request_version = Some(version);
                        open.request_trailers = header_pairs(&trailers);
                        open.record_request_chunk(&bytes);
                    }
                    let mut response = if path == "/encoded" {
                        let mut response = Response::new(full_body("opaque encoded bytes"));
                        response
                            .headers_mut()
                            .insert(header::CONTENT_ENCODING, "custom-coding".parse().unwrap());
                        response
                            .headers_mut()
                            .insert(header::CONTENT_TYPE, "text/html".parse().unwrap());
                        response
                    } else if path == "/empty" {
                        Response::new(empty_body())
                    } else if h2 {
                        let mut trailers = HeaderMap::new();
                        trailers.insert("grpc-status", "0".parse().unwrap());
                        let frames: Vec<Result<_, std::io::Error>> =
                            vec![Ok(Frame::data(bytes)), Ok(Frame::trailers(trailers))];
                        Response::new(BodyExt::boxed(StreamBody::new(futures::stream::iter(
                            frames,
                        ))))
                    } else {
                        Response::new(full_body(bytes))
                    };
                    response
                        .headers_mut()
                        .insert("x-origin", "local-fixture".parse().unwrap());
                    response
                        .headers_mut()
                        .insert(header::ALT_SVC, "h3=\":443\"".parse().unwrap());
                    Ok::<_, Infallible>(response)
                }
            });
            if h2 {
                let _ = http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            } else {
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            }
        });
        let session = Arc::new(Session {
            engine: Arc::new(FilterEngine::empty()),
            client: reqwest::Client::builder()
                .use_preconfigured_tls(tls.clone())
                .no_proxy()
                .build()
                .unwrap(),
            certs,
            exclusions: ExclusionStore::default(),
            // Ephemeral test ports serve HTTPS; explicitly opt them in.
            intercepts: ExclusionStore::new(vec!["localhost".into()]),
            state: Arc::new(ProxyState::new(MitmMode::Full)),
            tls_client_config: Arc::new(tls.clone()),
        });
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
        let shared = session.clone();
        let proxy_task = tokio::spawn(async move {
            let (stream, _) = proxy.accept().await.unwrap();
            let service = service_fn(move |request| handle(shared.clone(), request));
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades()
                .await;
        });
        let client = reqwest::Client::builder()
            .use_preconfigured_tls(tls)
            .proxy(reqwest::Proxy::all(proxy_url).unwrap())
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        Self {
            session,
            client,
            origin: format!("https://{authority}"),
            received,
            tasks: vec![origin_task, proxy_task],
        }
    }
}

#[tokio::test]
async fn negotiates_h2_on_both_sides_and_preserves_request_and_response_trailers() {
    let fixture = Harness::new(true).await;
    let mut trailers = HeaderMap::new();
    trailers.insert("x-client-trailer", "kept".parse().unwrap());
    let frames: Vec<Result<_, std::io::Error>> = vec![
        Ok(Frame::data(Bytes::from_static(b"hello h2"))),
        Ok(Frame::trailers(trailers)),
    ];
    let response = fixture
        .client
        .post(format!("{}/echo", fixture.origin))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(reqwest::Body::wrap(StreamBody::new(futures::stream::iter(
            frames,
        ))))
        .send()
        .await
        .unwrap();
    assert_eq!(response.version(), Version::HTTP_2);
    assert_eq!(response.headers()["x-origin"], "local-fixture");
    assert!(!response.headers().contains_key(header::ALT_SVC));
    let body = reqwest::Body::from(response).collect().await.unwrap();
    assert_eq!(body.trailers().unwrap()["grpc-status"], "0");
    assert_eq!(body.to_bytes(), "hello h2");
    let received = fixture.received.lock().unwrap();
    assert_eq!(received.request_version, Some(Version::HTTP_2));
    assert_eq!(
        received.request_trailers,
        vec![("x-client-trailer".into(), "kept".into())]
    );
    let events = fixture.session.state.recent_events(10);
    let open = events
        .iter()
        .find(|e| e.method == "POST")
        .unwrap()
        .exchange
        .lock()
        .unwrap();
    assert_eq!(open.request_version, Some(Version::HTTP_2));
    assert_eq!(open.response_version, Some(Version::HTTP_2));
    assert_eq!(
        open.request_body
            .snapshot()
            .read_range(0, usize::MAX)
            .unwrap(),
        b"hello h2"
    );
    assert_eq!(
        open.response_body
            .snapshot()
            .read_range(0, usize::MAX)
            .unwrap(),
        b"hello h2"
    );
    assert_eq!(
        open.response_trailers,
        vec![("grpc-status".into(), "0".into())]
    );
    assert!(open.error.is_none());
    assert!(open.finished_at.is_some());
    drop(open);
    let har = crate::proxy::har::build(&events);
    let post = har
        .log
        .entries
        .iter()
        .find(|e| e.request.method == "POST")
        .unwrap();
    assert_eq!(post.request.http_version, "HTTP/2");
    assert_eq!(post.response.http_version, "HTTP/2");
    assert_eq!(post.response.trailers[0].name, "grpc-status");
}

#[tokio::test]
async fn large_bodies_round_trip_over_http1_and_http2_and_are_fully_stored() {
    for h2 in [false, true] {
        let fixture = Harness::new(h2).await;
        let payload: Vec<u8> = (0..2 * 1024 * 1024 + 17).map(|i| (i % 251) as u8).collect();
        let response = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            fixture
                .client
                .post(format!("{}/echo", fixture.origin))
                .header("content-type", "application/octet-stream")
                .body(payload.clone())
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
        })
        .await
        .expect("disk backpressure must not deadlock HTTP flow control");
        assert_eq!(response.as_ref(), payload);
        let events = fixture.session.state.recent_events(10);
        let open = events
            .iter()
            .find(|event| event.method == "POST")
            .unwrap()
            .exchange
            .lock()
            .unwrap();
        for body in [&open.request_body, &open.response_body] {
            let snapshot = body.snapshot();
            snapshot.require_complete().unwrap();
            assert_eq!(snapshot.len, payload.len() as u64);
            assert_eq!(snapshot.read_range(0, payload.len()).unwrap(), payload);
        }
    }
}

#[tokio::test]
async fn http1_still_works_and_unknown_encoding_is_not_stripped_or_rewritten() {
    let fixture = Harness::new(false).await;
    let response = fixture
        .client
        .post(format!("{}/echo", fixture.origin))
        .body("hello h1")
        .send()
        .await
        .unwrap();
    assert_eq!(response.version(), Version::HTTP_11);
    assert_eq!(response.text().await.unwrap(), "hello h1");
    let response = fixture
        .client
        .get(format!("{}/encoded", fixture.origin))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.headers()[header::CONTENT_ENCODING],
        "custom-coding"
    );
    assert_eq!(response.text().await.unwrap(), "opaque encoded bytes");
    fixture
        .client
        .get(format!("{}/empty", fixture.origin))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let events = fixture.session.state.recent_events(10);
    let open = events
        .iter()
        .find(|e| e.url.ends_with("/empty"))
        .unwrap()
        .exchange
        .lock()
        .unwrap();
    assert_eq!(open.status, Some(200));
    assert!(open.finished_at.is_some());
    assert!(open.error.is_none());
}

#[tokio::test]
async fn oversized_html_starts_streaming_without_waiting_for_eof() {
    let fixture = Harness::new(false).await;
    let frames: Vec<Result<_, std::io::Error>> = vec![
        Ok(Frame::data(Bytes::from(vec![b'a'; MAX_REWRITABLE_BODY]))),
        Ok(Frame::data(Bytes::from_static(b"overflow"))),
    ];
    let stream = futures::stream::iter(frames).chain(futures::stream::pending());
    let response =
        reqwest::Response::from(Response::new(reqwest::Body::wrap(StreamBody::new(stream))));
    let exchange = Arc::new(Mutex::new(Exchange::for_test()));
    let mut body = tokio::time::timeout(
        Duration::from_secs(1),
        bounded_html_body(
            response,
            "https://localhost/large",
            &fixture.session,
            exchange.clone(),
        ),
    )
    .await
    .expect("must not collect an unbounded HTML stream");
    assert_eq!(
        body.frame()
            .await
            .unwrap()
            .unwrap()
            .data_ref()
            .unwrap()
            .len(),
        MAX_REWRITABLE_BODY
    );
    assert_eq!(
        body.frame().await.unwrap().unwrap().data_ref().unwrap(),
        b"overflow".as_slice()
    );
    let open = exchange.lock().unwrap();
    assert_eq!(open.response_body.seen(), (MAX_REWRITABLE_BODY + 8) as u64);
    assert_eq!(
        open.response_body.snapshot().len,
        (MAX_REWRITABLE_BODY + 8) as u64
    );
    assert!(open.finished_at.is_none());
}

#[test]
fn tls_failures_are_classified_without_guessing_certificate_rejection() {
    let rejected = std::io::Error::other(rustls::Error::AlertReceived(
        rustls::AlertDescription::UnknownCA,
    ));
    assert!(tls_failure(&rejected).starts_with("The client rejected"));
    let reset = std::io::Error::from(std::io::ErrorKind::UnexpectedEof);
    assert!(tls_failure(&reset).contains("does not prove certificate rejection"));
    let alpn = std::io::Error::other(rustls::Error::NoApplicationProtocol);
    assert!(tls_failure(&alpn).contains("negotiate HTTP/2"));
}

#[test]
fn connection_nominated_headers_are_removed_but_trailer_support_survives() {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONNECTION,
        "keep-alive, x-private-hop".parse().unwrap(),
    );
    headers.insert("x-private-hop", "remove me".parse().unwrap());
    headers.insert(header::TE, "trailers".parse().unwrap());
    headers.insert("x-end-to-end", "keep me".parse().unwrap());
    strip_hop_by_hop(&mut headers);
    assert!(!headers.contains_key(header::CONNECTION));
    assert!(!headers.contains_key("x-private-hop"));
    assert_eq!(headers[header::TE], "trailers");
    assert_eq!(headers["x-end-to-end"], "keep me");
}
