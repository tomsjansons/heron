--!nonstrict
-- Shared helpers for every heron entry.
--
-- Each entry runs in its own VM, so every entry gets its own copy of this
-- module; nothing here holds state. The service publishes one value on the
-- "heron" state key and every UI entry reads it through M.read():
--
--   {
--     status   = "connecting" | "connected" | "daemon_down" | "error",
--     error    = string?,          -- why, when not connected
--     snapshot = { ... }?,         -- normalized `heron watch` snapshot
--     updated  = number,           -- unix seconds of the last change
--   }
--
-- Actions (open, mark read/unread/seen/unseen, ack) and on-demand fetches
-- (`heron message`, `heron event`) run directly from the entry that needs them,
-- with the argv form of runAsync, so nothing goes through a shell.

local M = {}

M.STATE_KEY = "heron"
-- Development override for the `heron_command` setting, set over IPC on the
-- sync service (see README). In-memory only, gone after a shell restart.
M.COMMAND_KEY = "heron.command"
-- Panels write a fresh number here to make the service reconnect now.
M.RECONNECT_KEY = "heron.reconnect"

-- The inbox panel's selected account tab (an account id), remembered across
-- openings; the mail widget sets it to its own account before opening.
M.INBOX_TAB_KEY = "heron.inbox.tab"

M.PANEL_INBOX = "toms/heron:inbox"
M.PANEL_CALENDAR = "toms/heron:calendar"
M.PANEL_CHANNELS = "toms/heron:channels"

-- Bar glyph color while herond is unreachable or there is nothing to show.
M.DIM_ROLE = "on_surface_variant"

local ACTION_TIMEOUT_MS = 15000

-- ── Plain helpers ────────────────────────────────────────────────────────────

local function str(value)
  if type(value) == "string" then
    return value
  end
  return nil
end

local function num(value)
  if type(value) == "number" then
    return value
  end
  return 0
end

-- Index-based on purpose: two gsub passes over a 40 KB snapshot line blew the
-- stream callback's CPU budget (json.decode of the same line costs ~2 ms).
function M.trim(text)
  text = text or ""
  local first = string.find(text, "%S")
  if not first then
    return ""
  end
  local last = #text
  while last > first do
    local byte = string.byte(text, last)
    if byte == 32 or (byte >= 9 and byte <= 13) then
      last -= 1
    else
      break
    end
  end
  if first == 1 and last == #text then
    return text
  end
  return string.sub(text, first, last)
end

-- Cuts `text` to at most `max` characters, adding "…" when it was longer.
-- Byte-safe for invalid UTF-8 (falls back to bytes).
function M.ellipsize(text, max)
  if #text <= max then
    return text
  end
  local length = utf8.len(text)
  if length == nil then
    return string.sub(text, 1, max) .. "…"
  end
  if length <= max then
    return text
  end
  return string.sub(text, 1, utf8.offset(text, max + 1) - 1) .. "…"
end

-- Single-quote a string for /bin/sh.
function M.shellQuote(text)
  return "'" .. string.gsub(text, "'", "'\\''") .. "'"
end

-- ── Command and actions ──────────────────────────────────────────────────────

-- Where `cargo install` and friends put binaries, which the shell's PATH often
-- lacks. Tried in order when a bare command name is not on PATH.
local FALLBACK_DIRS = { "/.cargo/bin/", "/.local/bin/" }

-- A bare name that is not on PATH, resolved against FALLBACK_DIRS under $HOME.
local function resolve(command)
  if string.find(command, "/", 1, true) or noctalia.commandExists(command) then
    return command
  end
  local home = noctalia.getenv("HOME")
  if home then
    for _, dir in FALLBACK_DIRS do
      local path = home .. dir .. command
      if noctalia.fileExists(path) then
        return path
      end
    end
  end
  return command
end

-- The heron executable: the IPC override, else the plugin setting, else "heron".
-- A leading "~/" is expanded by hand; noctalia.expandPath would turn a bare
-- name into a plugin-relative path. A bare name falls back to ~/.cargo/bin
-- and ~/.local/bin when it is not on PATH.
function M.command()
  local command = noctalia.state.get(M.COMMAND_KEY)
  if type(command) ~= "string" or M.trim(command) == "" then
    command = noctalia.getConfig("heron_command")
  end
  if type(command) ~= "string" or M.trim(command) == "" then
    command = "heron"
  end
  command = M.trim(command)
  if string.sub(command, 1, 2) == "~/" then
    command = (noctalia.getenv("HOME") or "~") .. string.sub(command, 2)
  end
  return resolve(command)
end

-- Runs `heron <args...>`. Failures raise an error notification; onDone, when
-- given, gets the CommandResult either way.
function M.run(args, onDone)
  local argv = { M.command() }
  for _, arg in ipairs(args) do
    argv[#argv + 1] = arg
  end
  local started = noctalia.runAsync(argv, function(result)
    if result.exitCode ~= 0 or result.timedOut then
      local reason = M.trim(result.stderr)
      if reason == "" then
        reason = M.trim(result.stdout)
      end
      noctalia.notifyError(noctalia.tr("notify.action_failed", { action = table.concat(args, " ") }), reason)
    end
    if onDone then
      onDone(result)
    end
  end, ACTION_TIMEOUT_MS)
  if not started then
    noctalia.notifyError(noctalia.tr("notify.action_failed", { action = table.concat(args, " ") }), M.command())
  end
  return started
end

-- Runs `heron <args...>` for a command that prints one JSON object
-- (`message ... --json`, `event ... --json`). Calls onDone(decoded, nil) on
-- success and onDone(nil, reason) on failure, where reason comes from a
-- `{"type":"error"}` reply, stderr, or the exit status. No notification: the
-- caller shows the error inline. Decoding happens in this runAsync callback, not
-- in the sync service's stream callback, so a large body costs only this call.
function M.runJson(args, onDone, timeoutMs)
  local argv = { M.command() }
  for _, arg in ipairs(args) do
    argv[#argv + 1] = arg
  end
  local started = noctalia.runAsync(argv, function(result)
    if result.timedOut then
      onDone(nil, noctalia.tr("error.timed_out"))
      return
    end
    local out = M.trim(result.stdout)
    local decoded = nil
    if string.sub(out, 1, 1) == "{" then
      local newline = string.find(out, "\n", 1, true)
      local line = newline and string.sub(out, 1, newline - 1) or out
      local ok, value = pcall(noctalia.json.decode, line)
      if ok and type(value) == "table" then
        decoded = value
      end
    end
    if decoded and decoded.type == "error" then
      onDone(nil, type(decoded.message) == "string" and decoded.message or out)
    elseif decoded and result.exitCode == 0 then
      onDone(decoded, nil)
    else
      local reason = M.trim(result.stderr)
      if reason == "" then
        reason = out
      end
      reason = string.gsub(reason, "^Error:%s*", "")
      if reason == "" then
        reason = noctalia.tr("error.exit_code", { code = result.exitCode })
      end
      onDone(nil, M.ellipsize(reason, 300))
    end
  end, timeoutMs or 30000)
  if not started then
    onDone(nil, noctalia.tr("status.command_missing", { command = M.command() }))
  end
  return started
end

-- Fire-and-forget xdg-open. Only http(s) and mailto links are opened.
function M.openUrl(url)
  if type(url) ~= "string" then
    return false
  end
  if not (string.match(url, "^https?://") or string.match(url, "^mailto:")) then
    return false
  end
  return noctalia.runAsync({ "xdg-open", url })
end

-- ── Published state ──────────────────────────────────────────────────────────

function M.read()
  local value = noctalia.state.get(M.STATE_KEY)
  if type(value) == "table" then
    return value
  end
  return { status = "connecting" }
end

-- The snapshot when connected, else nil.
function M.snapshot(value)
  if value and value.status == "connected" and type(value.snapshot) == "table" then
    return value.snapshot
  end
  return nil
end

-- Tooltip/banner text for a non-connected state.
function M.statusText(value)
  local status = value and value.status or "connecting"
  if status == "daemon_down" then
    return noctalia.tr("status.daemon_down")
  elseif status == "error" then
    return noctalia.tr("status.error", { message = value.error or "?" })
  elseif status == "connected" then
    return noctalia.tr("status.connected")
  end
  return noctalia.tr("status.connecting")
end

-- ── Snapshot normalization ───────────────────────────────────────────────────
--
-- Turns a decoded `heron watch` line into plain tables with every field typed,
-- so JSON nulls (however the decoder represents them) never reach state or a
-- renderer.

local function normalizeMessage(raw)
  return {
    id = str(raw.id) or "",
    from = str(raw.from) or "",
    subject = str(raw.subject) or "",
    received = num(raw.received),
    unread = raw.unread == true,
    new = raw.new == true,
    url = str(raw.url) or "",
  }
end

local function normalizeAccount(raw)
  local id = str(raw.id) or ""
  local label = str(raw.label) or ""
  local messages = {}
  if type(raw.messages) == "table" then
    for _, message in ipairs(raw.messages) do
      if type(message) == "table" then
        messages[#messages + 1] = normalizeMessage(message)
      end
    end
  end
  return {
    id = id,
    label = label ~= "" and label or id,
    provider = str(raw.provider) or "",
    new = num(raw.new),
    unread = num(raw.unread),
    messages = messages,
  }
end

local function normalizeEvent(raw)
  return {
    id = str(raw.id) or "",
    source = str(raw.source) or "",
    calendar = str(raw.calendar) or "",
    title = str(raw.title) or "",
    location = str(raw.location) or "",
    start = num(raw.start),
    ["end"] = num(raw["end"]),
    all_day = raw.all_day == true,
    color = str(raw.color) or "",
    url = str(raw.url) or "",
    meeting_url = str(raw.meeting_url) or "",
  }
end

-- Discord previews: at most this many per channel and this many channels, so a
-- busy server cannot blow the sync callback's budget or the panel's row count.
local DISCORD_PREVIEWS = 3
local DISCORD_CHANNELS = 30
local DISCORD_CONTENT_MAX = 200

local function normalizeDiscordMessages(raw)
  local out = {}
  if type(raw) ~= "table" then
    return out
  end
  -- Oldest first from herond; keep the newest DISCORD_PREVIEWS.
  local first = math.max(1, #raw - DISCORD_PREVIEWS + 1)
  for index = first, #raw do
    local message = raw[index]
    if type(message) == "table" then
      out[#out + 1] = {
        id = str(message.id) or "",
        author = str(message.author) or "",
        content = M.ellipsize(str(message.content) or "", DISCORD_CONTENT_MAX),
        timestamp = num(message.timestamp),
        mentions_me = message.mentions_me == true,
      }
    end
  end
  return out
end

local function normalizeDiscord(raw)
  if type(raw) ~= "table" then
    return nil
  end
  local channels = {}
  local unread = 0
  local more = false
  if type(raw.channels) == "table" then
    for _, channel in ipairs(raw.channels) do
      if #channels >= DISCORD_CHANNELS then
        break
      end
      if type(channel) == "table" then
        local entry = {
          id = str(channel.id) or "",
          name = str(channel.name) or "",
          unread = num(channel.unread),
          more = channel.more == true,
          mentions = num(channel.mentions),
          url = str(channel.url) or "",
          messages = normalizeDiscordMessages(channel.messages),
        }
        channels[#channels + 1] = entry
        unread += entry.unread
        more = more or entry.more
      end
    end
  end
  local unreadMessages = num(raw.unread_messages)
  return {
    id = str(raw.id) or "",
    name = str(raw.name) or "",
    unread_channels = num(raw.unread_channels),
    -- herond before unread counts: fall back to the per-channel sum.
    unread_messages = unreadMessages > 0 and unreadMessages or unread,
    -- Some channel has more unread messages than its (capped) count says.
    more = more,
    mentions = num(raw.mentions),
    channels = channels,
  }
end

function M.normalize(raw)
  local snapshot = {
    new = num(raw.new),
    unread = num(raw.unread),
    mail = {},
    calendar = {},
    discord = normalizeDiscord(raw.discord),
    sources = {},
  }
  if type(raw.mail) == "table" then
    for _, account in ipairs(raw.mail) do
      if type(account) == "table" then
        snapshot.mail[#snapshot.mail + 1] = normalizeAccount(account)
      end
    end
  end
  if type(raw.calendar) == "table" then
    for _, event in ipairs(raw.calendar) do
      if type(event) == "table" then
        snapshot.calendar[#snapshot.calendar + 1] = normalizeEvent(event)
      end
    end
  end
  if type(raw.sources) == "table" then
    for _, source in ipairs(raw.sources) do
      if type(source) == "table" then
        snapshot.sources[#snapshot.sources + 1] = {
          id = str(source.id) or "",
          kind = str(source.kind) or "",
          state = str(source.state) or "",
          message = str(source.message) or "",
        }
      end
    end
  end
  return snapshot
end

-- ── Sources ──────────────────────────────────────────────────────────────────

local AUTH_PROVIDER = {
  gmail = "google",
  google_calendar = "google",
  proton_mail = "proton",
  proton_calendar = "proton",
  discord = "discord",
}

-- The `heron auth ...` command line for a source that needs a login.
function M.authCommand(source)
  local provider = AUTH_PROVIDER[source.kind] or source.kind
  if provider == "discord" then
    return "heron auth discord"
  end
  return "heron auth " .. provider .. " " .. source.id
end

-- Sources that are not ok, filtered to the given kinds (a set) when passed.
-- "connecting" is left out: it is transient and not worth a banner.
function M.problems(snapshot, kinds)
  local out = {}
  if not snapshot then
    return out
  end
  for _, source in ipairs(snapshot.sources) do
    if (source.state == "needs_auth" or source.state == "error") and (kinds == nil or kinds[source.kind]) then
      out[#out + 1] = source
    end
  end
  return out
end

-- One line describing a problem source.
function M.problemText(source)
  local name = noctalia.tr("kind." .. source.kind)
  if source.state == "needs_auth" then
    return noctalia.tr("source.needs_auth", { source = source.id, kind = name, command = M.authCommand(source) })
  end
  local message = source.message ~= "" and source.message or noctalia.tr("source.unknown_error")
  return noctalia.tr("source.error", { source = source.id, kind = name, message = message })
end

M.MAIL_KINDS = { gmail = true, proton_mail = true }
M.CALENDAR_KINDS = { google_calendar = true, proton_calendar = true }
M.DISCORD_KINDS = { discord = true }

-- ── Time ─────────────────────────────────────────────────────────────────────

function M.now()
  return os.time()
end

-- "14:00", following [shell].time_format.
function M.clock(unix)
  return noctalia.formatTime(noctalia.timeFormat(), unix)
end

function M.dayKey(unix)
  return noctalia.formatTime("%Y-%m-%d", unix)
end

-- "today" | "tomorrow" | "later" relative to now.
function M.dayBucket(unix, now)
  local key = M.dayKey(unix)
  if unix <= now or key == M.dayKey(now) then
    return "today"
  elseif key == M.dayKey(now + 86400) then
    return "tomorrow"
  end
  return "later"
end

-- "now", "5m", "3h", "2d", then a date.
function M.relative(unix, now)
  local delta = now - unix
  if delta < 60 then
    return noctalia.tr("time.now")
  elseif delta < 3600 then
    return noctalia.tr("time.minutes", { n = math.floor(delta / 60) })
  elseif delta < 86400 then
    return noctalia.tr("time.hours", { n = math.floor(delta / 3600) })
  elseif delta < 7 * 86400 then
    return noctalia.tr("time.days", { n = math.floor(delta / 86400) })
  end
  return noctalia.formatTime("%d %b", unix)
end

-- "14:00 – 15:00", or "all day".
function M.timeRange(event)
  if event.all_day then
    return noctalia.tr("calendar.all_day")
  end
  return M.clock(event.start) .. " – " .. M.clock(event["end"])
end

-- The timed event in progress, else the next timed event starting within
-- `horizon` seconds. All-day events are skipped. Returns event, ongoing.
function M.nextEvent(snapshot, now, horizon)
  if not snapshot then
    return nil, false
  end
  for _, event in ipairs(snapshot.calendar) do
    if not event.all_day and event["end"] > now and event.start - now <= horizon then
      return event, event.start <= now
    end
  end
  return nil, false
end

-- A JSON field that should be a string, else "".
function M.str(value)
  return str(value) or ""
end

-- A JSON field that should be a list of strings, else {}.
function M.strList(value)
  local out = {}
  if type(value) == "table" then
    for _, item in ipairs(value) do
      if type(item) == "string" and item ~= "" then
        out[#out + 1] = item
      end
    end
  end
  return out
end

-- "a, b, c and 4 more" for at most `max` items.
function M.joinList(items, max)
  if #items <= max then
    return table.concat(items, ", ")
  end
  local shown = table.move(items, 1, max, 1, {})
  return noctalia.tr("list.more", { items = table.concat(shown, ", "), count = #items - max })
end

-- Unread count as shown: "100+" when the count is capped.
function M.countText(count, more)
  if more then
    return tostring(count) .. "+"
  end
  return tostring(count)
end

-- Asks the service to reconnect, which re-reads the snapshot right away and
-- skips any pending backoff.
function M.requestReconnect()
  noctalia.state.set(M.RECONNECT_KEY, noctalia.nowMs())
end

-- A label in a fixed-width cell. A label's own `width` acts as a maximum, so
-- columns only line up when the label sits in a sized row.
function M.cell(width, props)
  return ui.row({ width = width, minWidth = width, align = "center" }, { ui.label(props) })
end

-- Renders `build()` into the panel, or the error on the surface when it throws.
function M.renderPanel(build)
  local ok, treeOrError = pcall(build)
  if ok then
    panel.render(treeOrError)
  else
    panel.render(ui.column({ flexGrow = 1, gap = 8, padding = 8 }, {
      ui.label({ text = noctalia.tr("panel.render_error"), fontWeight = "bold", color = "error" }),
      ui.label({ text = tostring(treeOrError), fontSize = 12, color = "on_surface" }),
    }))
  end
end

-- Shared panel header: glyph, title, summary, refresh and close buttons.
function M.header(glyph, title, summary, onRefresh, onCloseClicked)
  return ui.row({ align = "center", gap = 8 }, {
    ui.glyph({ name = glyph, size = 18, color = "primary" }),
    ui.label({ text = title, fontSize = 16, fontWeight = "bold" }),
    ui.label({ text = summary, fontSize = 11, color = "on_surface/0.6", flexGrow = 1, maxLines = 1 }),
    ui.button({
      glyph = "refresh",
      glyphSize = 14,
      variant = "ghost",
      tooltip = noctalia.tr("panel.refresh"),
      onClick = onRefresh,
    }),
    ui.button({
      glyph = "x",
      glyphSize = 14,
      variant = "ghost",
      tooltip = noctalia.tr("panel.close"),
      onClick = onCloseClicked,
    }),
  })
end

-- Banner for a disconnected daemon, or nil when connected.
function M.statusBanner(value)
  if value and value.status == "connected" then
    return nil
  end
  local glyph = value and value.status == "connecting" and "refresh" or "plug-connected-x"
  return ui.row({ gap = 8, padding = 10, radius = 8, align = "center", fill = "surface_variant/0.5" }, {
    ui.glyph({ name = glyph, size = 16, color = "on_surface/0.7" }),
    ui.label({ text = M.statusText(value), fontSize = 12, flexGrow = 1, maxLines = 3 }),
  })
end

-- One row per problem source.
function M.problemRows(problems)
  local rows = {}
  for _, source in ipairs(problems) do
    rows[#rows + 1] = ui.row({ key = "problem-" .. source.kind .. "-" .. source.id, gap = 8, paddingH = 8, paddingV = 4, align = "center" }, {
      ui.glyph({ name = "alert-triangle", size = 14, color = "error" }),
      ui.label({ text = M.problemText(source), fontSize = 11, flexGrow = 1, maxLines = 2, color = "on_surface/0.8" }),
    })
  end
  return rows
end

return M
