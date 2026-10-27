# heron

Watches your inboxes so your shell doesn't have to. `herond` keeps mail,
calendar and Discord state in sync; the `heron` CLI and a
[Noctalia](https://noctalia.dev) plugin show it in the bar.

Status: all sources are implemented and unit-tested against fixtures; none has
been exercised against a live account yet.

## Sources

| Source | How |
| --- | --- |
| Proton Mail | Native Proton API (no Bridge). Message metadata (sender, subject, unread) is not end-to-end encrypted, so listing needs only the SRP login. Changes arrive through the Proton event loop. |
| Proton Calendar | Native Proton API, read-only. Key chain: user key → address key → calendar passphrase → calendar key → event session key. Crypto from [`proton-crypto-rs`](https://github.com/ProtonMail/proton-crypto-rs). |
| Gmail | IMAP + IDLE with XOAUTH2. |
| Google Calendar | Calendar API v3 with incremental sync tokens, several accounts. |
| Discord | One server's unread channels, joined threads and mentions over the gateway. Channels can be marked read (`heron discord-read`). |

## New vs unread

- **unread**: the server says the message is unread. Marking a message unread
  in any client makes it unread here.
- **new**: unread and not yet acknowledged in the shell. Closing the inbox panel or
  running `heron ack` moves a per-account watermark, so a message stays unread
  but stops being new.

## Architecture

```
crates/
  heron-core     snapshot types, socket protocol, config, credentials, Hub
  heron-proton   Proton API: SRP login, mail event loop, calendar decryption
  heron-google   OAuth CLI login, Gmail IMAP IDLE, Calendar API
  heron-discord  user gateway, read state, permissions, mutes
  herond         one supervised task per source; socket at $XDG_RUNTIME_DIR/heron.sock
  heron          CLI: status | watch | message | event | ack | mark-(un)read | mark-(un)seen | trash | reload | auth | logout | init
plugin/          Noctalia plugin toms/heron (see plugin/README.md)
contrib/         systemd user unit, example config
```

Files: config `~/.config/heron/config.toml`, credentials `~/.local/share/heron/*.json`
(mode 0600; Proton stores the derived key passphrase, never the password),
acknowledgements `~/.local/state/heron/acks.json`.

## Setup

```sh
cargo install --path crates/herond
cargo install --path crates/heron


heron init                    # asks a few questions, writes ~/.config/heron/config.toml

heron auth google personal    # browser OAuth; see crates/heron-google/src/lib.rs for the Cloud setup
heron auth proton proton      # password, TOTP, mailbox password if two-password mode
heron auth discord            # paste your user token

mkdir -p ~/.config/systemd/user
cp contrib/herond.service ~/.config/systemd/user/
systemctl --user enable --now herond

heron status
```

Shell plugin:

```sh
ln -s ~/dev/heron/plugin ~/.local/share/noctalia/plugins/heron
noctalia msg plugins enable toms/heron
```

Set the plugin's **heron command** setting to `~/.cargo/bin/heron` (Noctalia's
PATH usually lacks `~/.cargo/bin`) and add the `toms/heron:mail`,
`toms/heron:agenda` and `toms/heron:discord` widgets to a bar.

## Caveats

- **Proton**: the API is undocumented. heron identifies itself honestly
  (`x-pm-appversion: Other`), which may make human verification more likely
  at login. Calendar signatures are not verified.
- **Discord**: uses your user token. That is against Discord's terms; you
  accept the risk. heron never sends messages or presence; the only write is
  marking channels read, and only when you ask (`heron discord-read`).
- **Google**: an OAuth app left in "Testing" gets refresh tokens that expire
  after 7 days; publish it ("In production") for your own use.

## License

GPL-3.0-only. See [LICENSE](LICENSE).
