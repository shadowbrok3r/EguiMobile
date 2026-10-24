# Changelog

## v0.5.6 — 2026-10-24

- Allow CONNECT before filtering inspected HTTPS requests, so blocked requests retain their URL and headers.
- Add shared theme presets with saved appearance settings.
- Add remembered desktop pairing, clipboard import, and an encrypted desktop-routing toggle. Requests appear in the desktop log while routing is enabled.
- Update dependencies.

Desktop routing requires the updated Privaxy desktop app. HTTPS inspection through desktop uses its inspection CA; pairing does not install that CA or bypass certificate pinning.
