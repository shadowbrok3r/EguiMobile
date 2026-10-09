# Privaxy for Android

An ad-blocking HTTP/HTTPS proxy with an egui front end, built on `egui-mobile`. The proxy core is
ported from the desktop [privaxy](https://github.com/Barre/privaxy) server — same `adblock` engine,
same CONNECT-interception shape — adapted to what Android actually allows. A `VpnService` claims
the device's default route and feeds every app's traffic into it, so it covers mobile data too.

```bash
cargo egui-mobile run -a --release      # from this directory
```

## What changed from the desktop server

| Desktop privaxy | Here | Why |
| --- | --- | --- |
| `openssl` (vendored) for the CA and leaf certs | `rcgen` + `ring` | Vendored OpenSSL means building the C library against the NDK sysroot. rcgen is pure Rust and cross-compiles with nothing extra. |
| RSA-2048 keys | ECDSA P-256 | Key generation drops from seconds to milliseconds, and a leaf is signed on the request path for every new host. |
| `~/.privaxy` via `dirs::home_dir` | app private files directory | Android has no home directory, and this is the only location writable without a storage permission. |
| `reqwest` default TLS | `rustls-no-provider` + `webpki-roots` | reqwest's `rustls` feature forces rustls-platform-verifier, which needs Android Java helper classes this APK does not bundle and panics uninitialized. |
| Engine on a thread behind a crossbeam channel | `RwLock<Engine>` | Matching only needs `&self`, so requests match concurrently and the HTML rewriter no longer has to `block_on` a channel round trip from inside a sync callback. |
| Whole response buffered in memory | streamed, with bounded HTML rewriting | HTML is buffered up to 8 MiB; larger documents stream unchanged. Unsupported content encodings are preserved and never rewritten. |
| Filter lists from `filters.privaxy.net` | upstream URLs directly | No single point of failure outside the user's control. Adds AdGuard Mobile Ads, which desktop lists largely miss. |
| Web UI on a second port | egui, in-process | — |

## The two modes, and why the default is what it is

**Hostname only (default).** TLS is never terminated. Blocked hosts are refused when the client
sends `CONNECT`, and everything else is tunneled byte for byte. Domain-anchored rules (`||host^`)
are the bulk of every filter list and match on hostname alone, so this blocks most ad and tracker
traffic. It needs no certificate, works for every app on the device, and does not break certificate
pinning.

**Full inspection.** Terminates TLS with a certificate minted from the local CA, so individual URLs
and page content can be filtered — this is where cosmetic filtering and HTML rewriting happen.

Installing the certificate does **not** turn inspection on. Apps targeting Android 7+ do not trust
user-installed CAs by default; individual apps can opt in or use their own trust store. Certificate
pinning is another independent restriction. Full inspection is therefore opt-in, and hosts whose
apps reject the CA go in **Never intercept**. See [Android's trust configuration](https://developer.android.com/privacy-and-security/security-config)
and [certificate pinning](https://docs.mitmproxy.org/stable/concepts/certificates/#certificate-pinning).

The dashboard and Settings check the exact current CA against Android's enabled certificate store,
including its validity dates. The check runs in the background, refreshes on return from Settings,
and can be repeated with **Check certificate again**. Missing or invalid certificates produce a
warning; an unavailable check stays neutral. An installed CA is not a promise that every app trusts
it: actual client rejections remain visible in **Requests > Failed**.

**Inspect these hosts** is the inverse, and usually the one you want. A host named there is
terminated even in hostname-only mode, so a handful of hosts can be read without putting every app
on the device behind a certificate they do not trust. Never intercept wins if a host is on both
lists. Both are edited as chips in Settings, or from the **Interception** card on any request.

### Diagnosing missing request details

The proxy negotiates HTTP/2 or HTTP/1.1 with both the client and origin. Request/response trailers
(including gRPC status) survive forwarding and appear in the inspector. Binary protobuf payloads
remain binary; the inspector does not decode their schema.
Inspected responses do not advertise `Alt-Svc` alternatives that could switch clients to QUIC;
keep **Drop QUIC** enabled for cached alternatives and clients that try HTTP/3 directly.

| What the log shows | Meaning / next action |
| --- | --- |
| TUNNEL / CONNECT | A working opaque connection. Install the current CA **and** enable full inspection or select its host. Never intercept takes precedence. |
| TLS / CONNECT | The TLS connection itself. Individual HTTP requests appear in separate rows; use **With details** or the **Inspectable** filter. |
| FAILED | Open Overview for the actual failure. Certificate alerts, protocol negotiation, resets, upstream errors and timeouts are distinguished. |
| Body details | Full request and response bodies are stored privately on disk. Page through them or use **Save body** / HAR export. An incomplete body has a specific storage or transport error. |
| No entry | Confirm capture is active, IPv6 is captured and **Drop QUIC** is enabled. UDP traffic is not HTTP-inspected; clients without TCP fallback may stop working if QUIC is dropped. |

Start with **Dashboard → Open HTTPS browser test** after adding `example.com` to **Inspect these
hosts**, then look for its GET row and client/origin protocols. That verifies capture and trust for
that browser only. For third-party apps that reject the CA, prefer selective host inspection and
Never intercept. For an app you develop, use Android's debug-only trust anchors. Regenerating the CA
will not fix pinning and requires reinstalling the new certificate.

Full inspection automatically attempts HTTPS on 443/8443. Other TCP ports remain tunnels unless
their host is explicitly selected, preserving protocols such as messaging and DNS-over-TLS.
Encrypted ClientHello can hide the true hostname, and non-HTTP TLS is not decoded. These limits
cannot be removed by installing a CA. Changes to inspection settings apply to **new** connections;
restart the client app or capture to close an existing pooled tunnel.

## Pointing traffic at it

**Capture (default path).** Dashboard → **Capture all traffic**. Android asks for VPN permission
once, then shows a key icon while it runs. This covers mobile data and apps that ignore the proxy
setting, because it is not a proxy setting — it is the device's default route.

**By hand.** Android has no system-wide proxy setting, so this is per Wi-Fi network:
Settings → Network & internet → Internet → gear icon on your network → Edit → Advanced → Proxy →
Manual → `127.0.0.1`, port `8100`. It does not cover mobile data, and apps are free to ignore it.

To install the CA for full inspection: Settings → Export certificate (shares the `.crt` out; save
it to the device), then Settings → Security → Encryption & credentials → Install a certificate →
CA certificate.

## Capturing everything

`VpnService` is the only way Android lets an app see traffic it did not originate. The service
claims a tun interface with a default route; `ipstack` turns the IP packets on it back into
streams; each stream is then spoken to the proxy already listening on loopback, exactly as a
client would. Blocking policy never moves — a captured flow becomes a `CONNECT`, the proxy answers
it the same way it answers the browser's, and there is still one request log.

**Naming the destination.** Packets carry an address; filter lists are written against hosts. In
order:

1. the TLS ClientHello's SNI, sniffed off connections to 443 or 8443, including hellos fragmented
   across TCP reads and TLS records (bounded to 64 KiB);
2. otherwise, the DNS reverse map — every answer forwarded through the tun records which addresses
   a name resolved to;
3. otherwise the address itself, which still matches IP-literal rules.

**Per protocol.**

| Flow | Handling |
| --- | --- |
| TCP port 80 | Relayed byte for byte. The proxy reads origin-form requests and takes the authority from `Host`, so plain HTTP is filtered by full URL, not just by host. |
| TCP anything else | `CONNECT host:port`, then the bytes. A refusal (blocked host) closes the flow, which the app sees as a reset connection. |
| UDP 53 | Forwarded, and the answers read into the reverse map on the way past. |
| UDP 443 | Dropped by default, so QUIC apps fall back to TCP where the proxy can see them. An HTTP proxy carries no datagrams, so the alternative is passing QUIC unfiltered. |
| Other UDP | Forwarded directly. |
| ICMP and the rest | Dropped. |

**Not capturing itself.** The proxy's own upstream connections must not come back through the tun,
or every proxied request would be fed into itself. `VpnService.Builder.addDisallowedApplication`
excludes this whole process, which covers reqwest, the filter downloads and the DNS forwarder
without any of them having to know a VPN exists — and needs no `protect()` call on each socket,
which `reqwest` gives no hook for anyway.

### `tun2proxy` or a stack of our own

`tun2proxy` is a CLI application shipped as a library: `clap` (with `wrap_help` and `color`),
`daemonize`, `ctrlc2`, `env_logger`, `dotenvy` and `windows-service` are all non-optional
dependencies, and on Android it carries its own `jni` and `android_logger` alongside the ones
`egui-android` already links. It also owns the tun device through the `tun` crate and expects to
be configured, not called.

`ipstack` is what tun2proxy uses underneath, and it is the part that was wanted: `ahash`,
`etherparse`, `log`, `rand`, `thiserror`, `tokio` — pure Rust, no cmake, no C. The glue on top is
about 300 lines and is where the SNI sniffing, the port-80 relay and the request-log integration
live, none of which tun2proxy's shape would have made easier. smoltcp was the other candidate and
sits a level lower still: it wants a fixed set of sockets, so accepting an arbitrary destination
means writing the address-rewriting layer that `ipstack` already is.

### Staying alive

The service also runs the foreground notification, with or without a tun, so Android stops
reclaiming the process when the app is backgrounded. There is no VPN foreground-service type:
`systemExempted` is reserved for an app the user has already selected under Settings → VPN, which
would throw for the notification-only mode, so the service declares `specialUse`. That is a Play
Store review category rather than a runtime one, which is fine for a sideload — a Play release
would also need the `PROPERTY_SPECIAL_USE_FGS_SUBTYPE` property, which the manifest generator has
no key for.

## Inspecting a request

Search, filters, pause recording, clear log and HAR export stay above the scrolling list in one
compact row. The icon order is **filter / pause / trash / download**. The header's separate
play/pause control starts or stops VPN capture from every screen; the toolbar's pause only stops
adding requests to the log.

Displayed rows stay still while new traffic arrives, including when sorting by size, duration or
domain. A **new requests** counter appears beside the refresh icon; tap it to load the latest log.
Search and filter changes also refresh the view. Opening a row reads the current exchange, and
returning keeps your place. Captured entries and their bodies remain available throughout the session. Search covers all
captured entries, with 100 rows per displayed page. New arrivals never move a displayed row.

The Requests tab searches URLs *and* header names and values, filters by outcome / status class /
method, sorts by time, duration, size or host, and optionally groups by registrable domain (busiest
first, so the noisiest third party is at the top). Tapping **Inspect** opens the exchange:
Overview, Request and Response, each with the headers as sent and the body.

How much there is to see follows directly from the interception mode:

| | Headers | Body |
| --- | --- | --- |
| Plain HTTP (port 80) | yes | yes |
| HTTPS, hostname-only mode | no — a `CONNECT` tunnel is opaque | no |
| HTTPS, full inspection | yes | yes |
| Blocked at `CONNECT` | no, nothing was sent | — |

Rows with nothing to show say why rather than rendering an empty page.

Bodies stream to files in the app's private storage. There is no 64 KiB body cap and no eviction
after 60 requests. Disk writes use background workers with backpressure; the request log keeps
metadata and file references rather than payload copies. Inspection loads 16 KiB pages on demand,
with a page number and first/last controls. JSON bodies up to 512 KiB also have the expandable tree.
**Save body** exports every stored byte; **Copy page** copies the displayed page.

A session is limited to **512 MiB of body data or 5,000 entries**. Reaching either limit pauses
recording new entries and displays an explanation. Existing bodies are preserved. If a body hits
the disk limit mid-stream, its retained bytes remain readable and its incomplete state is explicit;
traffic still flows. Save the capture and clear the log to free storage and continue. Manually
pausing recording also leaves existing data intact. Captures survive minimizing and Activity
recreation, but a process restart / Force Stop starts a new session; export anything to keep.

**Save capture** streams the whole log and its full stored bodies as HAR 1.2 in a background worker,
then offers Android's Downloads/share flow. Export does not load a session-sized JSON document
into memory. Chrome DevTools (Network → Import HAR), Charles and Fiddler can import it. Binary
responses use HAR base64; binary request bodies use a `_encoding` extension. Blocked and tunnelled
entries carry status `0` with the reason in `statusText`. Incomplete captures include the retained
and observed lengths plus the actual storage error in `comment`. HTTP versions and `_trailers`
are preserved. Replay streams the stored request body and records the streamed response; it will
not replay a body known to be incomplete.

### Returning to the app

The proxy backend is retained independently of the Activity, so replacing the UI does not tear
down the running proxy or erase the current capture. The Android bridge refreshes its Activity
and GL handles on recreation. Privaxy rebuilds its blur targets on resume instead of retaining
stale rendering resources. The local winit patch permits a replacement Activity to create its
own event loop in a process kept alive by the foreground service and exits it on Android Destroy.
The eframe patch handles cleanup after Suspend has already released the window. Overlapping
old and replacement activities borrow the same backend without creating a second proxy.

### Regression checks

Run `cargo test -p privaxy_android --lib` from the EguiMobile workspace. Local TLS fixtures verify
HTTP/2 on both sides, request/response trailers, HTTP/1.1 compatibility, empty responses, preserved
unknown content encodings, and bounded HTML streaming without waiting for EOF. Additional checks
cover fragmented ClientHello parsing, body retention and failure classification.

After ARTEMIS exploration, `tests/android_lifecycle.py` exercises repeated Home/resume and
Activity replacement while sending continuous proxy traffic. It requires ADB, Tesseract with
English data, an explicitly selected serial, capture already enabled, and an origin reachable
from the device. It asserts the process stays alive, the UI renders, and forwarding keeps working:

```bash
python3 tests/android_lifecycle.py --serial emulator-5554 \
  --origin http://10.0.2.2:18760/lifecycle --output /tmp/privaxy-lifecycle
```


### Reading a body

A body that parses as JSON is rendered by [`egui_json_tree`](https://crates.io/crates/egui_json_tree)
— collapsible per node, one level open by default so the shape is visible without walking a large
array. Anything else falls back to text, and text that is not text falls back to a hex dump. The
parse is cached against `(exchange, side, byte-length)`, so a settled body parses once and a
streaming one re-parses only as it grows.

**Headers** and **Body** are each collapsible, and collapsing one hands its height to the other.
**Wrap** switches both between wrapped text and a single scroller owning *both* axes. That last
detail is not cosmetic: a horizontal-only scroll area nested inside a vertical one registers a
drag-sense over its whole rect *before* its content, and ties in hit-testing go to the innermost
widget — so a vertical swipe over it scrolls neither area. Drag scrolling also never chains to a
parent when the inner area hits its end, which is why the inspector owns the central area outright
instead of living inside the page scroller.

## Blocking a host

Three routes to the same rule:

- **Block host** on any row in the log, **Block *.domain** in the inspector for a CDN that spreads
  across subdomains.
- **Filters → Block a domain** for one typed by hand (a pasted URL is reduced to its host).
- **Filters → Custom rules** for Adblock Plus syntax directly.

All of them add `||host^` — the domain anchor, which matches the host and every subdomain and is
the form the CONNECT-time check is built around, so it blocks in both interception modes. Adding a
rule rebuilds the engine from the on-disk cache; no list is re-downloaded.

Every one of them undoes: tapping the block icon on a blocked row unblocks it, and a rule in
**Filters → Custom rules** is removed by tapping its chip. For the other failure — a subscription
breaking a site you never blocked yourself — **Never block** in the inspector writes `@@||host^`,
which overrides the lists rather than removing a rule that was never yours.

## Working on a captured request

The inspector's **Actions** card operates on the whole exchange:

- **Replay this request** re-sends it on the proxy's own runtime and logs the result as a new row,
  so a before and after sit next to each other. The filter engine is deliberately skipped: a replay
  is explicit, and having it silently blocked by a rule would defeat the point.
- **Copy as cURL** produces a shell-quoted command — the phone-to-laptop handoff.

**Pause** stops adding new requests without touching traffic. Already captured bodies remain
available, and bodies of requests already in progress finish recording.

## Certificates

Two steps, because **Android will not let an app install a CA certificate**. `KeyChain
.createInstallIntent` is the documented route and it does open the system installer — which then
answers *"Can't install CA certificates — this certificate must be installed in Settings"* on
Android 11 and later, for any app that is not a device or profile owner. Tested on Android 17; the
capability is still in `egui-android` for client certificates and pre-API-30 devices, with that
caveat on the method.

So: **Save certificate to Downloads**, then **Open security settings** → Encryption & credentials →
Install a certificate → CA certificate → pick `privaxy-ca.crt` from Downloads.

Getting the file there was its own bug: `share_file` routed *every* file through a MediaStore
*Images* insert, and MediaProvider rejects a non-`image/*` MIME on that collection, so the JNI call
threw and `with_activity` swallowed it — the button had always done nothing.
`insert_into_media_store` now picks its collection from the MIME (gallery for media, `Download/`
for the rest), which also fixes the HAR export and comfyui-android's `.comfybk` share, both of
which sat on the same dead path. One wrinkle found on device: MediaProvider rewrites a filename
whose extension disagrees with its MIME, which turned `.har` into `.har.json` — so `.har` is
deliberately typed `application/octet-stream`, which Android leaves alone.

## Layout

```
src/
├── lib.rs              app! entry point
├── app.rs              lifecycle, tab chrome, lazily loaded state
├── proxy/
│   ├── mod.rs          Tokio runtime, listener, filter loading, ProxyHandle
│   ├── session.rs      CONNECT interception, tunneling, forwarding, HTML rewriting
│   ├── blocker.rs      adblock engine wrapper
│   ├── cert.rs         per-host leaf certificates, LRU cached
│   ├── ca.rs           the root certificate
│   ├── config.rs       settings, filter subscriptions, on-disk cache
│   ├── exclusions.rs   never-intercept hosts
│   └── state.rs        counters and request log shared with the UI
├── vpn/
│   ├── mod.rs          VpnService lifecycle, capture settings, status and counters
│   ├── relay.rs        ipstack accept loop, CONNECT client, port-80 relay, UDP
│   ├── tun.rs          AsyncRead/AsyncWrite over the tun descriptor
│   ├── sniff.rs        TLS ClientHello SNI
│   └── dns.rs          DNS answer parsing and the address → host map
└── ui/                 dashboard, requests, filters, settings
```

The Java half and the JNI bridge are shared, in `crates/egui-android`:
`java/com/github/egui_mobile/EguiVpnService.java` (tun, foreground notification), the
`onActivityResult` consent latch in `EguiNativeActivity.java`, and `src/vpn.rs`.

## Not done yet

- **`protect()` per socket.** Self-exclusion covers the loop today, which also means nothing this
  app sends is ever captured — including traffic a user might want to see in its own request log.
- **uBlock scriptlet and redirect resources.** Cosmetic *hiding* works; `##+js(...)` scriptlets and
  `$redirect=` rules need uBlock's `web_accessible_resources` embedded (~316K), plus adblock's
  `resource-assembler` feature.
- **DNS-level blocking.** The reverse map is read-only: answers are observed, never rewritten. It
  would block QUIC and other datagram traffic without dropping it, at the cost of putting blocking
  policy in a second place.
