--!nonstrict
-- Service entry `sync`: the only entry that keeps a process open.
--
-- Runs `heron watch` through runStream and republishes every snapshot on the
-- "heron" state key (see lib/heron.luau for the shape). When the stream ends,
-- because herond is not running or went away, it retries with backoff
-- (2s, 4s, ... 30s) and publishes `daemon_down` meanwhile.
--
-- runStream never reports its child exiting, so the stream is wrapped in a tiny
-- shell script: it prints the watch process's pid first (so a restart can kill
-- it) and a sentinel line with the exit code last.
--
-- Also raises notifications: one per batch of newly arrived *new* mail, and one
-- per calendar event shortly before it starts.
--
--   noctalia msg plugin toms/heron:sync all reconnect
--   noctalia msg plugin toms/heron:sync all command /path/to/heron   (dev override; "" clears)

local heron = require("./lib/heron.luau")

local PID_MARK = "__heron_pid__"
local EXIT_MARK = "__heron_exit__"
-- Messages per account requested from `heron watch`. Noctalia gives each
-- callback a small CPU budget; a full snapshot (50 per account) overran it.
local MAX_MESSAGES = 20
local BACKOFF_MIN = 2
local BACKOFF_MAX = 30
local TICK_IDLE_MS = 15000
local TICK_RETRY_MS = 1000
local SEEN_KEY = "heron.seen"
local REMINDED_KEY = "heron.reminded"

local generation = 0 -- bumps on every (re)start; lines from older streams are dropped
local running = false
local watchPid = nil
local gotSnapshot = false
local lastOutput = nil -- last non-JSON line, used as the error text
local backoff = BACKOFF_MIN
local retryAt = nil
local streamCommand = nil -- the command the current stream was started with

local status = "connecting"
local errorText = nil
local snapshot = nil

-- Both survive a service reload through noctalia.state, so a reload neither
-- re-announces mail nor repeats a reminder.
local seenNew = noctalia.state.get(SEEN_KEY) -- set of "account/id"; nil until the first snapshot
local reminded = noctalia.state.get(REMINDED_KEY) or {} -- event id -> start

local function publish()
  noctalia.state.set(heron.STATE_KEY, {
    status = status,
    error = errorText,
    snapshot = snapshot,
    updated = heron.now(),
  })
end

-- ── Notifications ────────────────────────────────────────────────────────────

local function notifyNewMail(current)
  local fresh = {}
  local nextSeen = {}
  for _, account in ipairs(current.mail) do
    for _, message in ipairs(account.messages) do
      if message.new then
        local key = account.id .. "/" .. message.id
        nextSeen[key] = true
        if seenNew ~= nil and not seenNew[key] then
          fresh[#fresh + 1] = message
        end
      end
    end
  end
  seenNew = nextSeen
  noctalia.state.set(SEEN_KEY, seenNew)

  if #fresh == 0 or noctalia.getConfig("notify_mail") == false then
    return
  end
  if #fresh == 1 then
    noctalia.notify(fresh[1].from, fresh[1].subject)
    return
  end
  local lines = {}
  for index, message in ipairs(fresh) do
    if index > 4 then
      lines[#lines + 1] = noctalia.tr("notify.more", { count = #fresh - 4 })
      break
    end
    lines[#lines + 1] = message.from .. ": " .. message.subject
  end
  noctalia.notify(noctalia.trp("notify.new_mail", #fresh), table.concat(lines, "\n"))
end

local function checkReminders(now)
  if not snapshot then
    return
  end
  local lead = tonumber(noctalia.getConfig("reminder_minutes")) or 0
  local changed = false
  if lead > 0 then
    for _, event in ipairs(snapshot.calendar) do
      local untilStart = event.start - now
      if not event.all_day and untilStart > 0 and untilStart <= lead * 60 and reminded[event.id] == nil then
        reminded[event.id] = event.start
        changed = true
        local body = heron.timeRange(event)
        if event.location ~= "" then
          body = body .. " · " .. event.location
        end
        noctalia.notify(
          noctalia.tr("notify.event_soon", { title = event.title, minutes = math.max(1, math.ceil(untilStart / 60)) }),
          body
        )
      end
    end
  end
  -- Forget reminders for events that ended long ago.
  for id, start in pairs(reminded) do
    if start < now - 86400 then
      reminded[id] = nil
      changed = true
    end
  end
  if changed then
    noctalia.state.set(REMINDED_KEY, reminded)
  end
end

-- ── Stream ───────────────────────────────────────────────────────────────────

local start -- forward declaration

local function scheduleRetry()
  retryAt = heron.now() + backoff
  backoff = math.min(backoff * 2, BACKOFF_MAX)
  noctalia.setUpdateInterval(TICK_RETRY_MS)
end

local function onSnapshot(raw)
  snapshot = heron.normalize(raw)
  status = "connected"
  errorText = nil
  gotSnapshot = true
  backoff = BACKOFF_MIN
  notifyNewMail(snapshot)
  checkReminders(heron.now())
  publish()
end

local function onStreamEnd(code)
  running = false
  watchPid = nil
  if gotSnapshot then
    backoff = BACKOFF_MIN -- it was healthy; retry quickly
  end
  local output = lastOutput or ""
  if code == 127 or string.find(output, "not found", 1, true) or string.find(output, "No such file", 1, true) then
    status = "error"
    errorText = noctalia.tr("status.command_missing", { command = heron.command() })
  elseif code ~= 0 and output ~= "" and not string.find(output, "not running", 1, true) then
    status = "error"
    errorText = output
  else
    status = "daemon_down"
    errorText = output ~= "" and output or nil
  end
  snapshot = nil
  publish()
  scheduleRetry()
end

local function onLine(gen, line)
  if gen ~= generation then
    return
  end
  line = heron.trim(line)
  if line == "" then
    return
  end
  local pid = string.match(line, "^" .. PID_MARK .. " (%d+)$")
  if pid then
    watchPid = pid
    return
  end
  local code = string.match(line, "^" .. EXIT_MARK .. " (%d+)$")
  if code then
    onStreamEnd(tonumber(code))
    return
  end
  if string.sub(line, 1, 1) == "{" then
    local decoded = noctalia.json.decode(line)
    if type(decoded) == "table" then
      if decoded.type == "snapshot" then
        local ok, err = pcall(onSnapshot, decoded)
        if not ok then
          noctalia.log("heron: bad snapshot: " .. tostring(err))
        end
        return
      elseif decoded.type == "error" then
        status = "error"
        errorText = type(decoded.message) == "string" and decoded.message or line
        publish()
        return
      end
    end
  end
  lastOutput = line
end

local function stopStream()
  generation += 1
  if watchPid then
    noctalia.runAsync({ "kill", watchPid })
  end
  watchPid = nil
  running = false
end

start = function()
  stopStream()
  local gen = generation
  streamCommand = heron.command()
  gotSnapshot = false
  lastOutput = nil
  retryAt = nil
  running = true
  if status ~= "connected" then
    status = "connecting"
    publish()
  end
  local script = string.format(
    "exec 2>&1; %s watch --messages %d & echo %s $!; wait $!; echo %s $?",
    heron.shellQuote(streamCommand),
    MAX_MESSAGES,
    PID_MARK,
    EXIT_MARK
  )
  local ok = noctalia.runStream(script, function(line)
    onLine(gen, line)
  end)
  if not ok then
    running = false
    status = "error"
    errorText = noctalia.tr("status.spawn_failed")
    publish()
    scheduleRetry()
  else
    noctalia.setUpdateInterval(TICK_IDLE_MS)
  end
end

local function reconnect()
  backoff = BACKOFF_MIN
  start()
end

-- ── Callbacks ────────────────────────────────────────────────────────────────

function update()
  local now = heron.now()
  if not running and retryAt ~= nil and now >= retryAt then
    start()
  end
  checkReminders(now)
end

function onConfigChanged()
  -- Only a changed command needs a new stream; the other settings are read live.
  if heron.command() ~= streamCommand then
    reconnect()
  end
end

function onIpc(event, payload)
  if event == "reconnect" then
    reconnect()
  elseif event == "command" then
    noctalia.state.set(heron.COMMAND_KEY, heron.trim(payload or ""))
    reconnect()
  end
end

function onExit(_signal, _reason)
  stopStream()
end

-- Refresh buttons in the panels. Ignore the value present at load, in case the
-- watch fires once on registration.
local lastReconnect = noctalia.state.get(heron.RECONNECT_KEY)
noctalia.state.watch(heron.RECONNECT_KEY, function(value)
  if value ~= nil and value ~= lastReconnect then
    lastReconnect = value
    reconnect()
  end
end)

publish()
start()
