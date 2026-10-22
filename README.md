# heron

Watches your inboxes so your shell doesn't have to. `herond` keeps mail,
calendar and Discord state in sync; the `heron` CLI and a
[Noctalia](https://noctalia.dev) plugin show it in the bar.

Status: early. The daemon, socket protocol and CLI exist; providers do not yet.

## Sources

| Source | How |
| --- | --- |
| Proton Mail | Native Proton API (no Bridge). Message metadata (sender, subject, unread) is not end-to-end encrypted, so listing needs only the SRP login. Changes arrive through the Proton event loop. |
| Proton Calendar | Native Proton API, read-only. Key chain: user key → address key → calendar passphrase → calendar key → event session key. Crypto from [`proton-crypto-rs`](https://github.com/ProtonMail/proton-crypto-rs). |
| Gmail | IMAP + IDLE with XOAUTH2. |
| Google Calendar | Calendar API v3 with incremental sync tokens, several accounts. |
| Discord | One server's unread channels and mentions over the gateway. |

## New vs unread

- **unread**: the server says the message is unread. Marking a message unread
  in any client makes it unread here.
- **new**: unread and not yet acknowledged in the shell. Opening the panel or
  running `heron ack` moves a per-account watermark, so a message stays unread
  but stops being new.

## Architecture

```
herond
  heron-core   snapshot types, socket protocol
  (planned)    proton, gmail, google-calendar, discord providers; SQLite store
  socket       $XDG_RUNTIME_DIR/heron.sock, line-delimited JSON

heron          CLI: status | watch | ack | mark-read | mark-unread
plugin/        Noctalia plugin: a service runs `heron watch` and publishes to
               noctalia.state; bar widgets and panels render from it
```

## Usage

```sh
cargo build --release
./target/release/herond &
heron status          # one JSON line
heron watch           # a JSON line now and on every change
```

## Plan

1. Skeleton: workspace, socket protocol, CLI. **Done.**
2. Proton: API client, SRP login with 2FA, session refresh, mail metadata
   through the event loop. Then the Noctalia service, mail widget and inbox
   panel.
3. Gmail: Google OAuth (one browser login per account), IMAP IDLE.
4. Calendars: Google Calendar API, then Proton Calendar decryption; agenda
   widget and calendar panel.
5. Discord: gateway client for one server, unread badge.
6. Hardening: reconnect and backoff, systemd user unit, plugin lint.

## License

GPL-3.0-only. See [LICENSE](LICENSE).
