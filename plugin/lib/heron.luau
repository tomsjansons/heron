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
-- Actions (open, mark read, ack) run directly from the entry that needs them,
-- with the argv form of runAsync, so nothing goes through a shell.

local M = {}

M.STATE_KEY = "heron"
-- Development override for the `heron_command` setting, set over IPC on the
-- sync service (see README). In-memory only, gone after a shell restart.
M.COMMAND_KEY = "heron.command"
-- Panels write a fresh number here to make the service reconnect now.
M.RECONNECT_KEY = "heron.reconnect"

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

function M.trim(text)
  return (string.gsub(text or "", "^%s+", ""):gsub("%s+$", ""))
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
  local messages = {}
  if type(raw.messages) == "table" then
    for _, message in ipairs(raw.messages) do
      if type(message) == "table" then
        messages[#messages + 1] = normalizeMessage(message)
      end
    end
  end
  return {
    id = str(raw.id) or "",
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
  }
end

local function normalizeDiscord(raw)
  if type(raw) ~= "table" then
    return nil
  end
  local channels = {}
  if type(raw.channels) == "table" then
    for _, channel in ipairs(raw.channels) do
      if type(channel) == "table" then
        channels[#channels + 1] = {
          id = str(channel.id) or "",
          name = str(channel.name) or "",
          mentions = num(channel.mentions),
          url = str(channel.url) or "",
        }
      end
    end
  end
  return {
    id = str(raw.id) or "",
    name = str(raw.name) or "",
    unread_channels = num(raw.unread_channels),
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

-- The ongoing event, else the next one starting within `horizon` seconds.
-- Ongoing all-day events lose to timed ones, which are more useful in a bar.
function M.nextEvent(snapshot, now, horizon)
  if not snapshot then
    return nil, false
  end
  local allDay = nil
  for _, event in ipairs(snapshot.calendar) do
    local ongoing = event.start <= now and event["end"] > now
    if ongoing and not event.all_day then
      return event, true
    elseif ongoing and allDay == nil then
      allDay = event
    end
  end
  for _, event in ipairs(snapshot.calendar) do
    if event.start > now and event.start - now <= horizon and not event.all_day then
      return event, false
    end
  end
  if allDay then
    return allDay, true
  end
  return nil, false
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
