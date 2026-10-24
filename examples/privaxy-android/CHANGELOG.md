# Changelog

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
