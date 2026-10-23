# Heron

Mail, calendar and Discord unread state in the Noctalia bar, from the
[heron](../README.md) daemon. A background service holds `heron watch` open and
publishes every snapshot; bar widgets and panels render from it.

## Plugin

| Field | Value |
| --- | --- |
| ID | `toms/heron` |
| Service | `sync` |
| Bar widgets | `mail`, `agenda`, `discord` |
| Panels | `inbox`, `calendar`, `channels` |

### Entries

- `sync` - service. Runs `heron watch`, republishes each snapshot on the
  `heron` state key with a connection status (`connecting`, `connected`,
  `daemon_down`, `error`). When the stream ends (herond stopped, restarted, not
  installed) it retries after 2 s, 4 s, 8 s ... up to 30 s. Raises a
  notification for newly arrived *new* mail (sender and subject, batched when
  several arrive together) and one per calendar event shortly before it starts.
- `mail` - bar widget. `(N) new; (M) unread` across every account. The glyph is
  primary while anything is new and dims while herond is unreachable; the
  tooltip has per-account counts and any account that needs a login. Click
  toggles the `inbox` panel.
- `agenda` - bar widget. The timed event in progress, or the next one within 24
  hours, as `13:00 - Standup`. All-day events are skipped (the panel shows them).
  Dims with no text when nothing is coming up. Click toggles the `calendar` panel.
- `discord` - bar widget. Unread channel count in the configured server, with
  `@N` when you have mentions. Click toggles the `channels` panel; right click
  opens the server in the browser.
- `inbox` - panel. Unread mail grouped by account: a dot (primary for new),
  sender, subject and age. Click a row to open the message; the envelope button
  marks it read or unread on the server. Accounts that need a login show the
  exact `heron auth ...` command to run.
- `calendar` - panel. Today / Tomorrow / Later groups with time range or "all
  day", title, calendar name with its color, and location. Ongoing events are
  highlighted; rows with a link open it.
- `channels` - panel. Unread channels with mention badges, most mentions first.
  Click a channel to open it; "Open server" opens the whole server.

Open the panels without a widget:

```sh
noctalia msg panel-toggle toms/heron:inbox
noctalia msg panel-toggle toms/heron:calendar
noctalia msg panel-toggle toms/heron:channels
```

### New vs unread

"New" is acknowledged when the `inbox` panel **closes** (`heron ack`, every
account), not when it opens. The dots stay visible while you read the list, and
the `(N) new` counter clears once you dismiss it. Acknowledging on open (after
a delay) would clear the dots before you had looked at them. The acknowledgement
moves herond's watermark to "now", so mail that arrives while the panel is open
is acknowledged too; its notification has already been shown by then. Nothing is
acknowledged when nothing was new.

## Requirements

- Noctalia with plugin API 24 or newer.
- `heron` (the CLI) and a running `herond` daemon from this repository. Only the
  `heron` binary is called directly; it talks to `herond` over its socket.
- `xdg-open` to open messages, events and channels in the browser.

## Setup

```sh
cargo build --release          # in the heron repo
ln -s ~/dev/heron/plugin ~/.local/share/noctalia/plugins/heron
noctalia msg plugins enable toms/heron
```

Noctalia's `PATH` usually lacks `~/.cargo/bin`. If the widgets say they cannot
run `heron`, set **heron command** under Settings → Plugins → Heron to an
absolute path such as `/home/you/.cargo/bin/heron` (a leading `~/` works too).

Add the widgets to a bar in `~/.config/noctalia/*.toml` (or through the
settings UI):

```toml
[widget.heron_mail]
type = "toms/heron:mail"

[widget.heron_agenda]
type = "toms/heron:agenda"

[widget.heron_discord]
type = "toms/heron:discord"

[bar.default]
end = ["heron_agenda", "heron_discord", "heron_mail"]
```

## Settings

- **heron command** (`heron_command`, default `heron`) - the CLI to run, by name
  or absolute path.
- **Notify on new mail** (`notify_mail`, default on) - one notification per
  batch of new mail.
- **Event reminder (minutes)** (`reminder_minutes`, default 10) - notify this
  long before a timed event starts, once per event. 0 turns reminders off.

## IPC

```sh
noctalia msg plugin toms/heron:sync all reconnect                 # reconnect now, skip the backoff
noctalia msg plugin toms/heron:sync all command /path/to/heron    # override heron_command in memory
noctalia msg plugin toms/heron:sync all command ""                # drop the override
```

The `command` override lives in `noctalia.state`, so it lasts until the shell
restarts and never touches your settings. Every entry uses it.

The refresh button in each panel does the same as `reconnect`.

## Development

`dev/fake-heron` is a stand-in for the CLI with fixture data: two mail accounts,
a calendar with an ongoing event, one about to start, tomorrow and later
events, a Discord server, and sources in `needs_auth` and `error` states. A new
message arrives every 20 seconds (five in total), and `ack`, `mark-read` and
`mark-unread` persist to `$XDG_RUNTIME_DIR/fake-heron.json`, so actions round-trip
through the stream.

```sh
noctalia msg plugin toms/heron:sync all command "$HOME/dev/heron/plugin/dev/fake-heron"
touch "$XDG_RUNTIME_DIR/fake-heron.down"    # simulate herond stopping
rm "$XDG_RUNTIME_DIR/fake-heron.down"       # and coming back
rm "$XDG_RUNTIME_DIR/fake-heron.json"       # reset the fixtures
cat "$XDG_RUNTIME_DIR/fake-heron.log"       # every invocation, with timestamps
```

Checks:

```sh
luau-compile --null *.luau lib/*.luau
noctalia plugins lint .
```

## Notes

- Processes: the `sync` service runs `/bin/sh -c "<heron> watch & ...; wait"`
  for as long as the plugin is enabled. The wrapper reports the watch pid (so a
  reconnect can `kill` it) and its exit code, because `runStream` does not
  report a child exiting. Panels run `heron ack`, `heron mark-read` and
  `heron mark-unread` with argv (no shell), and `xdg-open <url>` for http(s)
  and mailto links only.
- No network access and no filesystem writes from the plugin itself. State is
  in memory (`noctalia.state` keys `heron`, `heron.seen`, `heron.reminded`,
  `heron.command`, `heron.reconnect`).
- Rendering is guarded: a panel that fails to build shows the error text instead
  of an empty surface.

## License

GPL-3.0-only, like the rest of heron.
