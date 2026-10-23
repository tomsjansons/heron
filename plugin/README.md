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
| Panels | `inbox` (640×620), `calendar` (500×560), `channels` (460×560) |

### Entries

- `sync` - service. Runs `heron watch --messages 20`, republishes each snapshot
  on the `heron` state key with a connection status (`connecting`, `connected`,
  `daemon_down`, `error`). When the stream ends (herond stopped, restarted, not
  installed) it retries after 2 s, 4 s, 8 s ... up to 30 s. Raises a
  notification for newly arrived *new* mail (sender and subject, batched when
  several arrive together) and one per calendar event shortly before it starts.
  A message that becomes new again through **Mark unseen** is not announced.
- `mail` - bar widget. New/unread per account, using the account labels from
  herond's config: `proton 2/63 · work 0/55`. With the per-instance `account`
  setting it shows one account as `2/63` (label and counts in the tooltip), so
  you can put one widget per account on the bar. The glyph is primary while
  anything shown is new, red while a shown account needs a login, and dims while
  herond is unreachable. Click toggles the `inbox` panel, opened on the widget's
  account tab when it has one.
- `agenda` - bar widget. The timed event in progress, or the next one within 24
  hours, as `13:00 · Standup`. All-day events are skipped (the panel shows them).
  Dims with no text when nothing is coming up. Click toggles the `calendar` panel.
- `discord` - bar widget. Unread messages and the ones that notify you
  (mentions, reply pings, @everyone, roles) in the configured server: `12 · @3`.
  The count reads `112+` when a channel has more unread than herond counts
  (capped at 100 per channel), and `#4 · @3` (unread channels) with a herond
  that does not report message counts. Glyph and text turn primary while
  anything notifies you. Click toggles the `channels` panel; right click opens
  the server in the browser.
- `inbox` - panel, two views.
  - **List**: one tab per account (label and unread count, a dot while it has
    new mail; the selected tab is remembered), then that account's messages: a
    dot (primary for new), sender, subject and age. The envelope button marks a
    message read or unread on the server; the trash button moves it to the
    account's trash (`heron trash`). Accounts that need a login show the
    exact `heron auth ...` command to run.
  - **Preview**: click a message. Subject, from/to/cc/date, the body as plain
    text (`heron message <account> <id> --json --max-chars 6000`, which never
    marks it read), then its links as clickable rows. Buttons: **Mark read /
    Mark unread** (server), **Mark seen / Mark unseen** (the shell's "new" flag,
    see below), **Move to trash** (returns to the list) and **Open in browser**. Opening the preview marks the message
    seen, not read. Shows a loading state, and the error (with Retry) when the
    body cannot be fetched. **Back** returns to the list.
- `calendar` - panel. Today / Tomorrow / Later groups with time range or "all
  day", title, calendar name with its color, and location; a video glyph marks
  events with a meeting link. Ongoing events are highlighted. Clicking an event
  expands it inline and, when it has a meeting link, copies the link to the
  clipboard ("Meeting link copied" shows in the row). The expanded part comes
  from `heron event <id> --json`: location, meeting link, organizer, attendees
  and description (capped), with **Join** (opens the meeting link), **Copy
  link** and **Open in calendar** (the event's own link). Click again to
  collapse.
- `channels` - panel. Unread channels, most mentions first. Each channel header
  shows its unread count and @mentions; under it, previews of its newest unread
  messages (at most three: author, text, age), with messages that notify you
  highlighted. Click a channel or a preview to open the channel; its check
  button marks it read on Discord. "Mark all read" marks every listed channel
  read; "Open server" opens the whole server.

Open the panels without a widget:

```sh
noctalia msg panel-toggle toms/heron:inbox
noctalia msg panel-toggle toms/heron:calendar
noctalia msg panel-toggle toms/heron:channels
```

### New vs unread vs seen

*Unread* is the server's flag; **Mark read / Mark unread** change it.

*New* is the shell's own flag, kept by herond: unread mail that arrived after
the last acknowledgement. It drives the dots, the bar's first number and the
notifications. It changes three ways:

- **Closing the inbox** runs `heron ack` (every account) when anything was new.
  Acknowledging on close rather than on open keeps the dots visible while you
  read the list; they are gone the next time. The acknowledgement moves
  herond's watermark to "now", so mail that arrives while the panel is open is
  acknowledged too (its notification has already been shown by then).
- **Opening a preview** runs `heron mark-seen` for that message, which clears
  its "new" but leaves it unread.
- **Mark unseen** (`heron mark-unseen`) pins a message as new: it survives the
  ack on close until you mark it seen (or open its preview again, or mark it
  read). Use it to keep something on the bar as a reminder.

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

One mail widget per account instead of one for all of them:

```toml
[widget.heron_mail_proton]
type    = "toms/heron:mail"
account = "proton"      # account id or label from herond's config

[widget.heron_mail_work]
type    = "toms/heron:mail"
account = "work"
```

## Settings

- **heron command** (`heron_command`, default `heron`) - the CLI to run, by name
  or absolute path.
- **Notify on new mail** (`notify_mail`, default on) - one notification per
  batch of new mail.
- **Event reminder (minutes)** (`reminder_minutes`, default 10) - notify this
  long before a timed event starts, once per event. 0 turns reminders off.

Per `mail` widget instance:

- **Account** (`account`, default empty) - show only this account, by id or
  label, as `new/unread`. Empty shows every account.

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

`dev/fake-heron` is a stand-in for the CLI with fixture data: two mail accounts
(labels `home` and `proton`, one message an HTML-ish newsletter with links), a
calendar with an ongoing event, a stand-up with a Google Meet link, tomorrow and
later events, a Discord server with message previews and mentions (one channel
capped at `100+`), and sources in `needs_auth` and `error` states. It
implements `watch`, `status --json`, `ack`, `trash`, `mark-read`, `mark-unread`,
`mark-seen`, `mark-unseen`, `discord-read`, `message <account> <id> --json [--max-chars N]` and
`event <id> --json` (both with a short delay so loading states show;
`message personal m4` fails like an unsupported provider). A new message
arrives every 20 seconds (five in total). Actions persist to
`$XDG_RUNTIME_DIR/fake-heron.json`, so they round-trip through the stream.

```sh
noctalia msg plugin toms/heron:sync all command "$HOME/dev/heron/plugin/dev/fake-heron"
noctalia msg plugin toms/heron:sync all command ""   # back to the real heron

touch "$XDG_RUNTIME_DIR/fake-heron.quiet"   # stop new arrivals (and their notifications)
echo 40 > "$XDG_RUNTIME_DIR/fake-heron.big" # 40 busy Discord channels: a large snapshot
touch "$XDG_RUNTIME_DIR/fake-heron.down"    # simulate herond stopping
rm "$XDG_RUNTIME_DIR/fake-heron.down"       # and coming back
rm "$XDG_RUNTIME_DIR/fake-heron.json"       # reset the fixtures
cat "$XDG_RUNTIME_DIR/fake-heron.log"       # every invocation, with timestamps
```

A running watch keeps the fake-heron code it started with; reconnect after
editing it.

Checks:

```sh
luau-compile --null *.luau lib/*.luau
noctalia plugins lint .
```

## Notes

- Processes: the `sync` service runs `/bin/sh -c "<heron> watch --messages 20
  & ...; wait"` for as long as the plugin is enabled. The wrapper reports the
  watch pid (so a reconnect can `kill` it) and its exit code, because
  `runStream` does not report a child exiting. Panels run `heron ack`,
  `mark-read`, `mark-unread`, `mark-seen`, `mark-unseen`, `discord-read`,
  `message` and `event` with argv (no shell), and `xdg-open <url>` for http(s) and mailto links only.
  The calendar panel writes meeting links to the clipboard
  (`noctalia.copyToClipboard`).
- CPU budget: Noctalia gives each script callback a small CPU budget, and a
  callback that exceeds it is aborted silently. Measured on this plugin:
  `json.decode` is cheap (about 2 ms for 40 KB, 5 ms for 130 KB), but Luau
  `gsub` is not; a gsub-based trim of a 40 KB snapshot line, or tidying a 24 KB
  message body with gsub, ran over (about 20 ms). So the plugin trims and tidies
  by index and plain `find`, never with patterns over large text. The sync
  callback handles a 65 KB snapshot in about 7 ms. Lines longer than that reach
  `runStream`'s line limit and arrive in pieces, which are dropped, so the
  snapshot is kept small: `--messages 20` per account, and the service keeps at
  most 30 Discord channels with 3 previews each. Message bodies and event
  details are fetched on demand, decoded in their own `runAsync` callback, and
  capped (6000 characters of body, 40 links, 1500 characters of description).
- Filesystem and network: none from the plugin itself. State is in memory
  (`noctalia.state` keys `heron`, `heron.seen`, `heron.reminded`,
  `heron.command`, `heron.reconnect`, `heron.inbox.tab`).
- Labels only wrap when they have a width, so the inbox preview gives its text
  an explicit width; without one a long line would widen the panel.
- Rendering is guarded: a panel that fails to build shows the error text instead
  of an empty surface.

## License

GPL-3.0-only, like the rest of heron.
