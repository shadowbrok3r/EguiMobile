//! Turns captured IP packets into connections against the local proxy.
//!
//! [`ipstack`] runs a userspace TCP/IP stack over the tun descriptor and hands out one stream per
//! flow. Each stream is then spoken to the proxy the way a client would:
//!
//! - **port 80** — relayed byte for byte. The proxy reads origin-form requests and takes the
//!   authority from `Host`, so every request on the connection is filtered by full URL, not just
//!   by host.
//! - **everything else** — `CONNECT host:port`, then the bytes. The host comes from the TLS
//!   ClientHello where there is one, from the DNS reverse map otherwise, and finally from the
//!   destination address itself. A refusal from the proxy (a blocked host) closes the flow, which
//!   the app sees as a reset connection — the same thing it sees through the Wi-Fi proxy.
//! - **UDP** — forwarded directly, because an HTTP proxy carries no datagrams. Port 53 answers are
//!   read into the reverse map on the way past, and queries for filtered hosts are answered here.
//!   QUIC is either dropped so apps fall back to TCP, or named from its Initial packet, logged and
//!   blocked per host.
//!
//! Every flow is also attributed to the app that opened it, and the proxy learns that app through
//! [`ProxyState::expect_origin`] before the relay's connection reaches it.

use crate::proxy::state::{App, EventKind, NamedBy, Origin, ProxyState, RequestEvent};
use crate::vpn::apps::AppResolver;
use crate::vpn::dns::{self, DnsMap};
use crate::vpn::quic::{self, Sniffed};
use crate::vpn::tun::TunDevice;
use crate::vpn::{ProxyLink, VpnStats, sniff};
use ipstack::{
    IpStack, IpStackConfig, IpStackStream, IpStackTcpStream, IpStackUdpStream, TcpConfig,
};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf, copy_bidirectional};
use tokio::net::{TcpSocket, TcpStream, UdpSocket};
use tokio::sync::oneshot;

/// A phone's share of what a proxy on a laptop would take. Past this, flows are dropped rather
/// than queued: a dropped connection retries, an exhausted heap does not.
const MAX_TCP_FLOWS: usize = 512;
const MAX_UDP_FLOWS: usize = 256;
/// TCP flows close after this long without traffic. ipstack's own minute cuts push and chat
/// connections that only send a keep-alive every few minutes.
const TCP_IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60);

const PROXY_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Long enough for the proxy to answer CONNECT, which it does without touching the network.
const PROXY_REPLY_TIMEOUT: Duration = Duration::from_secs(20);
/// How long to wait for a ClientHello before giving up on the SNI and using the reverse map.
const SNIFF_TIMEOUT: Duration = Duration::from_secs(2);
/// A QUIC client sends its first flight at once; held datagrams only delay its handshake.
const QUIC_SNIFF_TIMEOUT: Duration = Duration::from_millis(500);
/// The proxy's CONNECT response headers; anything longer is not one.
const MAX_PROXY_REPLY: usize = 8192;
/// Resolvers retry and apps ask for A, AAAA and HTTPS at once; one row per name per window.
const DNS_ROW_WINDOW: Duration = Duration::from_secs(60);

const QUIC_BLOCKED_NOTE: &str = "QUIC to this host was dropped on its hostname. The app falls \
    back to TCP, where the same rules apply.";
const DNS_BLOCKED_NOTE: &str = "Answered with the unspecified address (0.0.0.0 or ::), so the \
    app never connected — over TCP, QUIC or anything else. Repeat lookups of this name within a \
    minute are answered the same way without new rows.";

#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// Where the proxy listens. Loopback, so it is never captured by the tun itself.
    pub proxy: SocketAddr,
    pub mtu: u16,
    /// Drop UDP 443 so QUIC apps fall back to TCP, where the proxy can see the traffic.
    pub block_quic: bool,
    /// Answer filtered hosts' DNS queries with the unspecified address.
    pub block_dns: bool,
    /// The resolver address handed to captured apps; only port 53 on it is served.
    pub virtual_dns: IpAddr,
    /// Where lookups sent to `virtual_dns` actually go.
    pub upstream_dns: Vec<IpAddr>,
}

/// Shared by every flow of one capture.
pub struct Context {
    config: RelayConfig,
    stats: Arc<VpnStats>,
    dns: Arc<DnsMap>,
    link: Arc<ProxyLink>,
    apps: Arc<AppResolver>,
    logged_dns_blocks: Mutex<HashMap<String, Instant>>,
    next_upstream: AtomicUsize,
}

impl Context {
    pub fn new(
        config: RelayConfig,
        stats: Arc<VpnStats>,
        dns: Arc<DnsMap>,
        link: Arc<ProxyLink>,
        apps: Arc<AppResolver>,
    ) -> Self {
        Self {
            config,
            stats,
            dns,
            link,
            apps,
            logged_dns_blocks: Mutex::new(HashMap::new()),
            next_upstream: AtomicUsize::new(0),
        }
    }

    /// Where a flow really goes: virtual resolver port 53 to an upstream, its other ports nowhere.
    fn route(&self, destination: SocketAddr) -> Option<SocketAddr> {
        if destination.ip() != self.config.virtual_dns {
            return Some(destination);
        }
        if destination.port() != 53 || self.config.upstream_dns.is_empty() {
            return None;
        }
        let turn = self.next_upstream.fetch_add(1, Ordering::Relaxed);
        let upstream = self.config.upstream_dns[turn % self.config.upstream_dns.len()];
        Some(SocketAddr::new(upstream, 53))
    }

    /// True for the first block of `name` in a window.
    fn first_dns_block(&self, name: &str) -> bool {
        let Ok(mut logged) = self.logged_dns_blocks.lock() else {
            return true;
        };
        let now = Instant::now();
        if logged.len() > 1024 {
            logged.retain(|_, at| now.duration_since(*at) < DNS_ROW_WINDOW);
        }
        match logged.get(name) {
            Some(at) if now.duration_since(*at) < DNS_ROW_WINDOW => false,
            _ => {
                logged.insert(name.to_owned(), now);
                true
            }
        }
    }
}

/// Owns the tun descriptor until `shutdown` fires or the interface goes away. Returns why it
/// stopped, or `None` when it was asked to.
pub async fn run(
    fd: OwnedFd,
    context: Arc<Context>,
    mut shutdown: oneshot::Receiver<()>,
) -> Option<String> {
    let device = match TunDevice::new(fd) {
        Ok(device) => device,
        Err(error) => return Some(format!("Could not open the tun interface: {error}")),
    };

    let mut stack_config = IpStackConfig::default();
    stack_config.packet_information(false);
    stack_config.mtu_unchecked(context.config.mtu);
    let mut tcp = TcpConfig::default();
    tcp.timeout = TCP_IDLE_TIMEOUT;
    stack_config.with_tcp_config(tcp);
    let mut stack = IpStack::new(stack_config, device);

    let tcp_flows = Arc::new(AtomicUsize::new(0));
    let udp_flows = Arc::new(AtomicUsize::new(0));
    let mut flows = tokio::task::JoinSet::new();
    log::info!("Privaxy VPN relaying to {}", context.config.proxy);

    let result = loop {
        while flows.try_join_next().is_some() {}
        let stream = tokio::select! {
            _ = &mut shutdown => break None,
            accepted = stack.accept() => match accepted {
                Ok(stream) => stream,
                Err(error) => {
                    log::warn!("VPN stack stopped: {error}");
                    break Some(format!("The VPN interface closed: {error}"));
                }
            },
        };

        match stream {
            IpStackStream::Tcp(tcp) => {
                if tcp_flows.load(Ordering::Relaxed) >= MAX_TCP_FLOWS {
                    log::warn!("VPN connection limit reached, dropping flow");
                    continue;
                }
                tcp_flows.fetch_add(1, Ordering::Relaxed);
                context.stats.tcp_flows.fetch_add(1, Ordering::Relaxed);

                let context = context.clone();
                let counter = tcp_flows.clone();
                flows.spawn(async move {
                    if let Err(error) = tcp_flow(tcp, context).await {
                        log::debug!("VPN tcp flow ended: {error}");
                    }
                    counter.fetch_sub(1, Ordering::Relaxed);
                });
            }
            IpStackStream::Udp(udp) => {
                if udp_flows.load(Ordering::Relaxed) >= MAX_UDP_FLOWS {
                    continue;
                }
                udp_flows.fetch_add(1, Ordering::Relaxed);
                context.stats.udp_flows.fetch_add(1, Ordering::Relaxed);

                let context = context.clone();
                let counter = udp_flows.clone();
                flows.spawn(async move {
                    if let Err(error) = udp_flow(udp, context).await {
                        log::debug!("VPN udp flow ended: {error}");
                    }
                    counter.fetch_sub(1, Ordering::Relaxed);
                });
            }
            // ICMP, IGMP and anything unparseable. Nothing here can be proxied, and answering
            // would only make the phone look like a router it is not.
            IpStackStream::UnknownTransport(_) | IpStackStream::UnknownNetwork(_) => {}
        }
    };
    // IpStackTcpStream::drop waits for its own task. Keep the executor running
    // until every stream has finished dropping, including queued accepts.
    // Shutting the runtime down first deadlocks that wait and leaks the tun.
    drop(stack);
    flows.abort_all();
    while flows.join_next().await.is_some() {}
    result
}

async fn tcp_flow(mut tcp: IpStackTcpStream, context: Arc<Context>) -> std::io::Result<()> {
    let source = tcp.local_addr();
    let destination = tcp.peer_addr();
    let Some(target) = context.route(destination) else {
        return Ok(());
    };
    let owner = context.apps.clone().resolve(false, source, destination);

    // Sniffed before the proxy is dialled: a connection opened first would sit against the
    // proxy's connection limit for as long as the client took to send its hello.
    let (head, app) = if sniff::TLS_PORTS.contains(&destination.port()) {
        let (head, app) = tokio::join!(read_client_hello(&mut tcp), owner);
        (head?, app)
    } else {
        (Vec::new(), owner.await)
    };

    // Port 80 is named by each request's Host header, inside the proxy.
    let named = (target.port() != 80)
        .then(|| name(sniff::server_name(&head), target.ip(), &context.dns));
    let origin = Arc::new(Origin {
        app,
        destination,
        named_by: named.as_ref().map_or(NamedBy::Host, |(_, by)| *by),
    });
    let session = context.link.session();
    let registration = session.as_ref().map(|session| (&session.state, &origin));
    let mut upstream = match dial_proxy(context.config.proxy, registration).await {
        Ok(stream) => stream,
        Err(error) => {
            context.stats.refused.fetch_add(1, Ordering::Relaxed);
            return Err(error);
        }
    };
    let _ = upstream.set_nodelay(true);

    let Some((host, _)) = named else {
        // Straight through: the proxy's own HTTP parser reads the host off each request.
        copy_bidirectional(&mut tcp, &mut upstream).await?;
        return Ok(());
    };

    match connect_through_proxy(&mut upstream, &host, target.port(), origin.app.as_deref()).await {
        Ok(leftover) => {
            if !leftover.is_empty() {
                tcp.write_all(&leftover).await?;
            }
        }
        Err(error) => {
            // A blocked host lands here. Dropping the stream resets the app's connection, which
            // is what a refused CONNECT looks like through the Wi-Fi proxy too.
            context.stats.refused.fetch_add(1, Ordering::Relaxed);
            log::debug!("VPN refused {host}:{}: {error}", target.port());
            return Ok(());
        }
    }

    if !head.is_empty() {
        upstream.write_all(&head).await?;
    }
    copy_bidirectional(&mut tcp, &mut upstream).await?;
    Ok(())
}

/// The host a flow is logged and filtered under, and where it came from.
fn name(handshake: Option<String>, address: IpAddr, dns: &DnsMap) -> (String, NamedBy) {
    if let Some(host) = handshake {
        return (host, NamedBy::Handshake);
    }
    if let Some(host) = dns.lookup(address) {
        return (host, NamedBy::Dns);
    }
    (address.to_string(), NamedBy::Address)
}

/// Connects to the proxy from a port registered with `origin` first, so the proxy can attribute
/// the connection as soon as it accepts it.
async fn dial_proxy(
    proxy: SocketAddr,
    registration: Option<(&Arc<ProxyState>, &Arc<Origin>)>,
) -> std::io::Result<TcpStream> {
    let socket = if proxy.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    socket.bind(SocketAddr::new(proxy.ip(), 0))?;
    let port = socket.local_addr()?.port();
    if let Some((state, origin)) = registration {
        state.expect_origin(port, origin.clone());
    }

    let failure = match tokio::time::timeout(PROXY_CONNECT_TIMEOUT, socket.connect(proxy)).await {
        Ok(Ok(stream)) => return Ok(stream),
        Ok(Err(error)) => error,
        Err(_) => std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "the proxy did not accept the connection",
        ),
    };
    if let Some((state, _)) = registration {
        state.take_origin(port);
    }
    Err(failure)
}

/// Read until the ClientHello is whole, the client turns out not to be speaking TLS, or the
/// deadline passes. Whatever was read is returned so it can be replayed to the proxy.
async fn read_client_hello(tcp: &mut IpStackTcpStream) -> std::io::Result<Vec<u8>> {
    let mut head = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 2048];
    let deadline = tokio::time::Instant::now() + SNIFF_TIMEOUT;

    loop {
        if !sniff::looks_like_client_hello(&head) || head.len() >= sniff::MAX_HELLO {
            return Ok(head);
        }
        // A complete hello without SNI (IP literals, for example) needs no extra timeout.
        if sniff::client_hello_complete(&head) {
            return Ok(head);
        }

        let room = chunk.len().min(sniff::MAX_HELLO - head.len());
        let read = tokio::time::timeout_at(deadline, tcp.read(&mut chunk[..room])).await;
        match read {
            Ok(Ok(0)) => return Ok(head),
            Ok(Ok(count)) => head.extend_from_slice(&chunk[..count]),
            Ok(Err(error)) => return Err(error),
            // No hello in time — a protocol where the server speaks first, or a slow client.
            Err(_) => return Ok(head),
        }
    }
}

/// Open a tunnel through the proxy. Returns any bytes read past the response headers, which
/// already belong to the tunnel.
async fn connect_through_proxy(
    upstream: &mut TcpStream,
    host: &str,
    port: u16,
    app: Option<&App>,
) -> std::io::Result<Vec<u8>> {
    let request = connect_request(&authority(host, port), app);
    upstream.write_all(&request).await?;

    let mut reply = Vec::with_capacity(128);
    let mut chunk = [0_u8; 1024];
    let header_end = loop {
        if let Some(at) = find_header_end(&reply) {
            break at;
        }
        if reply.len() >= MAX_PROXY_REPLY {
            return Err(std::io::Error::other(
                "the proxy's reply had no end of headers",
            ));
        }
        match tokio::time::timeout(PROXY_REPLY_TIMEOUT, upstream.read(&mut chunk)).await {
            Ok(Ok(0)) => return Err(std::io::Error::other("the proxy closed the connection")),
            Ok(Ok(count)) => reply.extend_from_slice(&chunk[..count]),
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(std::io::Error::other("the proxy did not answer CONNECT")),
        }
    };

    let status = status_code(&reply)
        .ok_or_else(|| std::io::Error::other("the proxy's reply was not HTTP"))?;
    if !(200..300).contains(&status) {
        return Err(std::io::Error::other(format!(
            "the proxy answered {status}"
        )));
    }

    Ok(reply.split_off(header_end))
}

/// CONNECT for `authority`, naming the app behind it so a paired desktop can decide whether to
/// decrypt it; the phone's own proxy ignores the extra headers.
fn connect_request(authority: &str, app: Option<&App>) -> Vec<u8> {
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n").into_bytes();
    let headers = app.into_iter().flat_map(|app| {
        [
            ("Privaxy-App", app.package.as_deref()),
            ("Privaxy-App-Name", app.label.as_deref()),
        ]
    });
    for (name, value) in headers {
        let Some(value) = value else { continue };
        let value: String = value.chars().filter(|c| !c.is_control()).collect();
        if !value.trim().is_empty() {
            request.extend_from_slice(format!("{name}: {}\r\n", value.trim()).as_bytes());
        }
    }
    request.extend_from_slice(b"\r\n");
    request
}

/// `host:port`, with an IPv6 literal bracketed the way a URI authority needs.
fn authority(host: &str, port: u16) -> String {
    if host.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Index just past the blank line ending the response headers.
fn find_header_end(reply: &[u8]) -> Option<usize> {
    reply
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|at| at + 4)
}

fn status_code(reply: &[u8]) -> Option<u16> {
    let line = reply.split(|byte| *byte == b'\r').next()?;
    let line = std::str::from_utf8(line).ok()?;
    if !line.starts_with("HTTP/") {
        return None;
    }
    line.split_whitespace().nth(1)?.parse().ok()
}

async fn udp_flow(udp: IpStackUdpStream, context: Arc<Context>) -> std::io::Result<()> {
    let source = udp.local_addr();
    let destination = udp.peer_addr();

    if destination.port() == 443 && context.config.block_quic {
        context.stats.quic_dropped.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    let Some(target) = context.route(destination) else {
        return Ok(());
    };

    let (mut from_app, to_app) = tokio::io::split(udp);
    let mut app_datagram = vec![0_u8; usize::from(context.config.mtu)];
    let mut held = Vec::new();
    let mut row = None;

    if destination.port() == 443 {
        let (datagrams, server_name) = sniff_quic(&mut from_app, &mut app_datagram).await?;
        if datagrams.is_empty() {
            return Ok(());
        }
        held = datagrams;

        let (host, named_by) = name(server_name, destination.ip(), &context.dns);
        let app = context.apps.clone().resolve(true, source, destination).await;
        let origin = Some(Arc::new(Origin {
            app,
            destination,
            named_by,
        }));
        let url = format!("https://{}/", authority(&host, destination.port()));
        let session = context.link.session();
        if let Some(session) = &session
            && let Some(rule) = session.quic_block(&host)
        {
            context.stats.quic_dropped.fetch_add(1, Ordering::Relaxed);
            session.state.record(
                RequestEvent::now("QUIC", &url, EventKind::Blocked { rule })
                    .with_origin(origin)
                    .note(QUIC_BLOCKED_NOTE),
            );
            // Retransmissions end here; dropping the flow would log each as a new one.
            while matches!(from_app.read(&mut app_datagram).await, Ok(count) if count > 0) {}
            return Ok(());
        }
        context.stats.quic_passed.fetch_add(1, Ordering::Relaxed);
        row = session.map(|session| {
            session.state.record(
                RequestEvent::now("QUIC", &url, EventKind::Tunneled)
                    .with_origin(origin)
                    .note(quic_note(named_by)),
            )
        });
    }

    // This process is excluded from the VPN, so the socket reaches the network directly instead
    // of being captured and fed back into the tun.
    let bind: SocketAddr = match target.ip() {
        IpAddr::V4(_) => ([0, 0, 0, 0], 0).into(),
        IpAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = UdpSocket::bind(bind).await?;
    socket.connect(target).await?;
    for datagram in &held {
        socket.send(datagram).await?;
    }

    let relayed = relay_datagrams(
        &context,
        &socket,
        from_app,
        to_app,
        app_datagram,
        source,
        destination,
    )
    .await;
    if let Some(row) = row
        && let Ok(mut open) = row.lock()
    {
        open.finished_at = Some(chrono::Local::now());
    }
    relayed
}

/// Holds a QUIC flow's first datagrams until its ClientHello names the host, or proves it cannot.
async fn sniff_quic(
    from_app: &mut ReadHalf<IpStackUdpStream>,
    buffer: &mut [u8],
) -> std::io::Result<(Vec<Vec<u8>>, Option<String>)> {
    let mut sniffer = quic::Sniffer::new();
    let mut held = Vec::new();
    let deadline = tokio::time::Instant::now() + QUIC_SNIFF_TIMEOUT;
    loop {
        let count = match tokio::time::timeout_at(deadline, from_app.read(buffer)).await {
            Ok(read) => read?,
            Err(_) => return Ok((held, None)),
        };
        if count == 0 {
            return Ok((held, None));
        }
        held.push(buffer[..count].to_vec());
        match sniffer.push(&buffer[..count]) {
            Sniffed::More => {}
            Sniffed::Hello(hello) => return Ok((held, sniff::hello_server_name(&hello))),
            Sniffed::Unreadable => return Ok((held, None)),
        }
    }
}

fn quic_note(named_by: NamedBy) -> String {
    let named = match named_by {
        NamedBy::Handshake => "read from its encrypted Initial packet",
        NamedBy::Dns => "taken from an earlier DNS answer; the QUIC handshake could not be read",
        _ => "unknown; neither the QUIC handshake nor DNS named it",
    };
    format!(
        "QUIC (HTTP/3) runs over UDP, which an HTTP proxy cannot carry, so this flow passed \
         through uninspected. The hostname was {named}. Turn on Drop QUIC in Settings to move \
         apps onto TCP, where requests can be inspected."
    )
}

async fn relay_datagrams(
    context: &Arc<Context>,
    socket: &UdpSocket,
    mut from_app: ReadHalf<IpStackUdpStream>,
    mut to_app: WriteHalf<IpStackUdpStream>,
    mut app_datagram: Vec<u8>,
    source: SocketAddr,
    destination: SocketAddr,
) -> std::io::Result<()> {
    let is_dns = destination.port() == 53;
    let mut net_datagram = vec![0_u8; 65_535];

    loop {
        tokio::select! {
            read = from_app.read(&mut app_datagram) => {
                // A read of nothing means the flow was torn down; ipstack's idle timeout errors.
                let count = read?;
                if count == 0 {
                    return Ok(());
                }
                let datagram = &app_datagram[..count];
                if is_dns {
                    context.stats.dns_queries.fetch_add(1, Ordering::Relaxed);
                    if let Some(answer) = answer_blocked(context, datagram, source, destination).await {
                        to_app.write_all(&answer).await?;
                        continue;
                    }
                }
                socket.send(datagram).await?;
            }
            received = socket.recv(&mut net_datagram) => {
                let count = received?;
                if is_dns {
                    context.dns.observe_response(&net_datagram[..count]);
                }
                to_app.write_all(&net_datagram[..count]).await?;
            }
        }
    }
}

/// The local answer to a query for a filtered host, logging the first per window.
async fn answer_blocked(
    context: &Arc<Context>,
    datagram: &[u8],
    source: SocketAddr,
    destination: SocketAddr,
) -> Option<Vec<u8>> {
    if !context.config.block_dns {
        return None;
    }
    let query = dns::parse_query(datagram)?;
    let session = context.link.session()?;
    let rule = session.dns_block(&query.name)?;
    context.stats.dns_blocked.fetch_add(1, Ordering::Relaxed);
    if context.first_dns_block(&query.name) {
        let app = context.apps.clone().resolve(true, source, destination).await;
        let origin = Arc::new(Origin {
            app,
            destination,
            named_by: NamedBy::Query,
        });
        session.state.record(
            RequestEvent::now("DNS", format!("dns://{}/", query.name), EventKind::Blocked { rule })
                .with_origin(Some(origin))
                .note(DNS_BLOCKED_NOTE),
        );
    }
    Some(dns::blocked_response(datagram, &query))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brackets_ipv6_authorities() {
        assert_eq!(authority("example.com", 443), "example.com:443");
        assert_eq!(authority("93.184.216.34", 443), "93.184.216.34:443");
        assert_eq!(authority("2606:4700::1111", 443), "[2606:4700::1111]:443");
    }

    #[test]
    fn connect_names_the_app_one_header_line_each() {
        let app = App {
            uid: 10123,
            package: Some("com.instagram.android".into()),
            label: Some("Insta\r\ngram ✨".into()),
        };
        assert_eq!(
            String::from_utf8(connect_request("i.instagram.com:443", Some(&app))).unwrap(),
            "CONNECT i.instagram.com:443 HTTP/1.1\r\nHost: i.instagram.com:443\r\n\
             Privaxy-App: com.instagram.android\r\nPrivaxy-App-Name: Instagram ✨\r\n\r\n"
        );
        let unnamed = App {
            uid: 1000,
            package: None,
            label: None,
        };
        for app in [None, Some(&unnamed)] {
            assert_eq!(
                connect_request("example.com:443", app),
                b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n"
            );
        }
    }

    #[test]
    fn reads_the_proxy_status() {
        assert_eq!(status_code(b"HTTP/1.1 200 OK\r\n\r\n"), Some(200));
        assert_eq!(status_code(b"HTTP/1.1 403 Forbidden\r\n\r\n"), Some(403));
        assert_eq!(status_code(b"garbage\r\n"), None);
    }

    #[test]
    fn splits_tunnel_bytes_off_the_reply() {
        let reply = b"HTTP/1.1 200 OK\r\n\r\n\x16\x03\x01".to_vec();
        let end = find_header_end(&reply).unwrap();
        assert_eq!(&reply[end..], b"\x16\x03\x01");
        assert_eq!(find_header_end(b"HTTP/1.1 200 OK\r\n"), None);
    }

    #[test]
    fn names_prefer_the_handshake_then_dns_then_the_address() {
        let map = DnsMap::new(8, Duration::from_secs(60));
        let address = IpAddr::from([10, 0, 0, 1]);
        assert_eq!(
            name(None, address, &map),
            (String::from("10.0.0.1"), NamedBy::Address)
        );
        let mut answer = vec![0x12, 0x34, 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0];
        answer.extend(b"\x03cdn\x07example\x03com\x00\x00\x01\x00\x01");
        answer.extend([0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 1, 0x2c, 0, 4, 10, 0, 0, 1]);
        map.observe_response(&answer);
        assert_eq!(
            name(None, address, &map),
            (String::from("cdn.example.com"), NamedBy::Dns)
        );
        assert_eq!(
            name(Some("api.example.com".into()), address, &map),
            (String::from("api.example.com"), NamedBy::Handshake)
        );
    }

    /// An IPv4 UDP datagram as the tun would carry it; ipstack does not check checksums.
    fn udp_packet(
        source: std::net::SocketAddrV4,
        destination: std::net::SocketAddrV4,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut packet = vec![0x45, 0, 0, 0, 0, 0, 0x40, 0, 64, 17, 0, 0];
        packet[2..4].copy_from_slice(&((28 + payload.len()) as u16).to_be_bytes());
        packet.extend(source.ip().octets());
        packet.extend(destination.ip().octets());
        packet.extend(source.port().to_be_bytes());
        packet.extend(destination.port().to_be_bytes());
        packet.extend(((8 + payload.len()) as u16).to_be_bytes());
        packet.extend([0, 0]);
        packet.extend(payload);
        packet
    }

    fn session(state: Arc<ProxyState>) -> Arc<crate::proxy::session::Session> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let ca = crate::proxy::ca::CertAuthority::generate().unwrap();
        Arc::new(crate::proxy::session::Session {
            engine: Arc::new(crate::proxy::blocker::FilterEngine::empty()),
            client: reqwest::Client::new(),
            certs: crate::proxy::cert::CertCache::new(&ca).unwrap(),
            exclusions: Default::default(),
            intercepts: Default::default(),
            state,
            tls_client_config: Arc::new(
                rustls::ClientConfig::builder()
                    .with_root_certificates(rustls::RootCertStore::empty())
                    .with_no_client_auth(),
            ),
            desktop_routing: false,
        })
    }

    #[test]
    fn filtered_dns_and_quic_are_answered_and_logged_through_the_tun() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let state = Arc::new(ProxyState::new(crate::proxy::config::MitmMode::HostnameOnly));
            let session = session(state.clone());
            session
                .engine
                .replace(vec![("Test".into(), "||example.com^".into())]);
            let link = Arc::new(ProxyLink::default());
            link.set(Some(session));
            let context = Arc::new(Context::new(
                RelayConfig {
                    proxy: ([127, 0, 0, 1], 9).into(),
                    mtu: 1500,
                    block_quic: false,
                    block_dns: true,
                    virtual_dns: crate::vpn::VIRTUAL_DNS.into(),
                    upstream_dns: vec![IpAddr::from([127, 0, 0, 1])],
                },
                Arc::default(),
                Arc::new(DnsMap::new(8, Duration::from_secs(60))),
                link,
                Arc::new(AppResolver::new()),
            ));
            let (device, peer) = std::os::unix::net::UnixDatagram::pair().unwrap();
            peer.set_nonblocking(true).unwrap();
            let peer = tokio::net::UnixDatagram::from_std(peer).unwrap();
            let (stop, shutdown) = oneshot::channel();
            let relay = tokio::spawn(run(OwnedFd::from(device), context.clone(), shutdown));

            let app = "10.7.0.1".parse().unwrap();
            let resolver = std::net::SocketAddrV4::new(crate::vpn::VIRTUAL_DNS, 53);
            let mut query = vec![0x51, 0x7a, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
            query.extend(b"\x03www\x07example\x03com\x00\x00\x01\x00\x01");
            let packet = udp_packet(std::net::SocketAddrV4::new(app, 40000), resolver, &query);
            peer.send(&packet).await.unwrap();
            let mut reply = vec![0_u8; 2048];
            let count = tokio::time::timeout(Duration::from_secs(3), peer.recv(&mut reply))
                .await
                .expect("the blocked query is answered locally")
                .unwrap();
            let answer = dns::parse_response(&reply[28..count]).unwrap();
            assert_eq!(answer.name, "www.example.com");
            assert_eq!(answer.addresses, vec![IpAddr::from([0, 0, 0, 0])]);

            let initial = crate::vpn::quic::tests::hex(
                crate::vpn::quic::tests::RFC9001_CLIENT_INITIAL,
            );
            let server = std::net::SocketAddrV4::new("127.0.0.1".parse().unwrap(), 443);
            let packet = udp_packet(std::net::SocketAddrV4::new(app, 40001), server, &initial);
            peer.send(&packet).await.unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            let quic = loop {
                let events = state.recent_events(10);
                if let Some(event) = events.into_iter().find(|event| event.method == "QUIC") {
                    break event;
                }
                assert!(Instant::now() < deadline, "no QUIC row was logged");
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            assert_eq!(quic.url, "https://example.com:443/");
            assert!(matches!(quic.kind, EventKind::Blocked { .. }));
            assert_eq!(quic.origin.as_ref().unwrap().named_by, NamedBy::Handshake);

            let events = state.recent_events(10);
            let dns_row = events.iter().find(|event| event.method == "DNS").unwrap();
            assert_eq!(dns_row.url, "dns://www.example.com/");
            assert!(matches!(&dns_row.kind, EventKind::Blocked { rule } if rule.text == "||example.com^"));
            let stats = context.stats.snapshot();
            assert_eq!((stats.dns_blocked, stats.quic_dropped, stats.quic_passed), (1, 1, 0));

            let _ = stop.send(());
            relay.await.unwrap();
        });
    }

    #[test]
    fn blocked_names_log_once_per_window_and_the_virtual_resolver_routes_upstream() {
        let context = Context::new(
            RelayConfig {
                proxy: ([127, 0, 0, 1], 9).into(),
                mtu: 1500,
                block_quic: false,
                block_dns: true,
                virtual_dns: crate::vpn::VIRTUAL_DNS.into(),
                upstream_dns: vec![IpAddr::from([1, 1, 1, 1]), IpAddr::from([1, 0, 0, 1])],
            },
            Arc::default(),
            Arc::new(DnsMap::new(8, Duration::from_secs(60))),
            Arc::default(),
            Arc::new(AppResolver::new()),
        );
        assert!(context.first_dns_block("ads.example.com"));
        assert!(!context.first_dns_block("ads.example.com"));
        assert!(context.first_dns_block("tracker.example.com"));

        // The virtual resolver serves port 53 from the upstreams in turn and refuses the rest.
        let virtual_dns = IpAddr::from(crate::vpn::VIRTUAL_DNS);
        let first = context.route(SocketAddr::new(virtual_dns, 53)).unwrap();
        let second = context.route(SocketAddr::new(virtual_dns, 53)).unwrap();
        assert_eq!(first, SocketAddr::from(([1, 1, 1, 1], 53)));
        assert_eq!(second, SocketAddr::from(([1, 0, 0, 1], 53)));
        assert_eq!(context.route(SocketAddr::new(virtual_dns, 853)), None);
        let elsewhere = SocketAddr::from(([8, 8, 8, 8], 853));
        assert_eq!(context.route(elsewhere), Some(elsewhere));
    }
}
