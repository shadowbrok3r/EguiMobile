# Changelog

## v0.6.2 — 2026-10-26

- Tell the paired desktop which app opened each routed connection, so it can decrypt browsers and pass other apps such as Instagram straight through instead of failing their first connection.

## v0.6.1 — 2026-10-26

- Move navigation from the scrolling top strip to an icon-only bottom bar that hides while typing, and make the Privaxy title a button back to the Dashboard.
- Put the proxy settings first in Settings, move theme and diagnostic logs to the bottom, and shorten the explanations.
- Keep each screen's scroll position when switching tabs.

## v0.6.0 — 2026-10-24

- Name the app behind every captured connection (Android 10+), on rows, in the inspector, in search and as `_app` in HAR exports.
- With Drop QUIC off, name QUIC flows from their handshake, log them and block them per host instead of passing them unfiltered.
- Block filtered hosts at DNS: their lookups get 0.0.0.0, which stops QUIC and other UDP too. On by default; inspected hosts are left to URL rules.
- Show the list and line of the rule behind every block.
- Update the filter engine to adblock 0.13: about 6x less memory with the default lists, faster list rebuilds and matching, and `$method=` rules.
- Stream HTML pages as they arrive instead of buffering them for cosmetic filtering; pages over 8 MiB get hiding rules from their first 8 MiB.
- Show the dialled address and how the hostname was found in the inspector.
- Open the request filter panel fully on screen, and stop the scrolling tab bar from tripping the overflow guard.
- Resolve captured apps through a resolver inside the VPN. Android's automatic Private DNS and Chrome's secure DNS had been moving lookups to encrypted DNS, which left hostnames unattributed and DNS blocking blind.

## v0.5.12 — 2026-10-24

- Keep scroll handles visible with a wider, contrasting track in requests and inspection.
- Explain which CA is needed when routing through a desktop, with installation steps on the phone.
- Show desktop inspection guidance instead of local-only inspection controls when desktop routing is active.

## v0.5.8 — 2026-10-24

- Move desktop pairing status into the top row alongside Privaxy and the capture controls, removing the separate status row.
- Keep the compact status indicator tappable to open Pairing.

## v0.5.7 — 2026-10-24

- Open Privaxy directly from desktop pairing QR links, with a review before pairing.
- Edit a saved desktop IP/hostname and relay port on mobile.
- Show authenticated live connection status on both apps, including idle connections, disconnects and automatic reconnects.
- Move navigation and the pairing indicator into the top bar.
- Bound proxy/VPN startup, allow retries after listener failure, and retry Android callback registration failures.
- Fix a VPN shutdown deadlock that kept the network interface open after stopping capture with active TCP flows.
- Explain Tailscale proxy-only mode and Android’s single-VPN limit.

Update the desktop app too for the heartbeat indicator. Existing pairing codes remain supported.

## v0.5.6 — 2026-10-24

- Allow CONNECT before filtering inspected HTTPS requests, so blocked requests retain their URL and headers.
- Add shared theme presets with saved appearance settings.
- Add remembered desktop pairing, clipboard import, and an encrypted desktop-routing toggle. Requests appear in the desktop log while routing is enabled.
- Update dependencies.

Desktop routing requires the updated Privaxy desktop app. HTTPS inspection through desktop uses its inspection CA; pairing does not install that CA or bypass certificate pinning.
