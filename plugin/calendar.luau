--!nonstrict
-- Panel entry `calendar`: upcoming events in Today / Tomorrow / Later groups.
--
--   noctalia msg panel-toggle toms/heron:calendar
--
-- Each row shows the time range (or "all day"), the title, the calendar name
-- next to its color swatch, and the location; a video glyph marks events with a
-- meeting link. Ongoing events are highlighted.
--
-- Clicking a row expands it inline and, when the event has a meeting link,
-- copies that link to the clipboard ("Meeting link copied" shows in the row).
-- The expanded part is fetched on demand with `heron event <id> --json`
-- (organizer, attendees, description), in its own runAsync callback, and
-- cached while the panel is open. It has "Join" (opens the meeting link),
-- "Copy link" and "Open in calendar" (the event's own link) buttons. Clicking
-- the row again collapses it.
--
-- While open it checks every second and re-renders when the minute changes, so
-- "ongoing" stays honest.

local heron = require("./lib/heron.luau")

local DESCRIPTION_MAX = 1500
local ATTENDEES_MAX = 8

local isOpen = false
local hoveredKey = nil
local renderedMinute = nil
local current = heron.read()

local expandedId = nil
local copiedId = nil -- event whose meeting link was just copied
-- event id -> { loading = bool, error = string?, organizer, attendees, description, meeting_url, location, url }
local details = {}

local render -- forward declaration

-- ── Details ──────────────────────────────────────────────────────────────────

local function fetchDetails(event)
  local id = event.id
  local entry = details[id]
  if entry and (entry.loading or not entry.error) then
    return
  end
  details[id] = { loading = true }
  heron.runJson({ "event", id, "--json" }, function(decoded, err)
    if err then
      details[id] = { error = err }
    else
      local extra = type(decoded.details) == "table" and decoded.details or {}
      details[id] = {
        organizer = heron.str(extra.organizer),
        attendees = heron.strList(extra.attendees),
        description = heron.ellipsize(heron.trim(heron.str(extra.description)), DESCRIPTION_MAX),
        meeting_url = heron.str(decoded.meeting_url),
        location = heron.str(decoded.location),
        url = heron.str(decoded.url),
      }
    end
    render()
  end)
end

-- The meeting link from the snapshot, else from fetched details.
local function meetingUrl(event)
  if event.meeting_url ~= "" then
    return event.meeting_url
  end
  local entry = details[event.id]
  return entry and entry.meeting_url or ""
end

local function copyMeetingLink(event)
  local url = meetingUrl(event)
  if url ~= "" and noctalia.copyToClipboard(url, "text/plain") then
    copiedId = event.id
    return true
  end
  return false
end

local function onEventClicked(event)
  if expandedId == event.id then
    expandedId = nil
    copiedId = nil
  else
    expandedId = event.id
    copiedId = nil
    copyMeetingLink(event)
    fetchDetails(event)
  end
  render()
end

-- ── Rendering ────────────────────────────────────────────────────────────────

local function detailLine(glyph, text, maxLines)
  return ui.row({ gap = 8, align = "start" }, {
    ui.glyph({ name = glyph, size = 13, color = "on_surface/0.55" }),
    ui.label({ text = text, fontSize = 11, color = "on_surface/0.85", flexGrow = 1, maxLines = maxLines or 1 }),
  })
end

local function expandedView(event)
  local entry = details[event.id] or {}
  local url = meetingUrl(event)
  local lines = {}

  if copiedId == event.id then
    lines[#lines + 1] = ui.row({ gap = 6, align = "center" }, {
      ui.glyph({ name = "clipboard-check", size = 13, color = "primary" }),
      ui.label({ text = noctalia.tr("calendar.link_copied"), fontSize = 11, fontWeight = "bold", color = "primary" }),
    })
  end

  if entry.loading then
    lines[#lines + 1] = ui.label({ text = noctalia.tr("panel.loading"), fontSize = 11, color = "on_surface/0.55" })
  elseif entry.error then
    lines[#lines + 1] = ui.row({ gap = 8, align = "center" }, {
      ui.glyph({ name = "alert-triangle", size = 13, color = "error" }),
      ui.label({
        text = noctalia.tr("calendar.details_failed", { message = entry.error }),
        fontSize = 11,
        color = "on_surface/0.8",
        flexGrow = 1,
        maxLines = 3,
      }),
      ui.button({
        text = noctalia.tr("panel.retry"),
        variant = "ghost",
        fontSize = 11,
        onClick = function()
          fetchDetails(event)
          render()
        end,
      }),
    })
  end

  local location = event.location ~= "" and event.location or (entry.location or "")
  if location ~= "" then
    lines[#lines + 1] = detailLine("map-pin", location, 2)
  end
  if url ~= "" then
    lines[#lines + 1] = detailLine("video", url, 1)
  end
  if entry.organizer and entry.organizer ~= "" then
    lines[#lines + 1] = detailLine("user", noctalia.tr("calendar.organizer", { name = entry.organizer }), 1)
  end
  if entry.attendees and #entry.attendees > 0 then
    lines[#lines + 1] = detailLine("users", heron.joinList(entry.attendees, ATTENDEES_MAX), 3)
  end
  if entry.description and entry.description ~= "" then
    lines[#lines + 1] = ui.label({
      text = entry.description,
      fontSize = 11,
      color = "on_surface/0.75",
      maxLines = 14,
    })
  end

  local buttons = {}
  if url ~= "" then
    buttons[#buttons + 1] = ui.button({
      text = noctalia.tr("calendar.join"),
      glyph = "video",
      glyphSize = 14,
      variant = "primary",
      onClick = function()
        heron.openUrl(url)
      end,
    })
    buttons[#buttons + 1] = ui.button({
      text = noctalia.tr("calendar.copy_link"),
      glyph = "copy",
      glyphSize = 14,
      variant = "ghost",
      onClick = function()
        copyMeetingLink(event)
        render()
      end,
    })
  end
  local eventUrl = event.url ~= "" and event.url or (entry.url or "")
  if eventUrl ~= "" and eventUrl ~= url then
    buttons[#buttons + 1] = ui.button({
      text = noctalia.tr("calendar.open_in_calendar"),
      glyph = "external-link",
      glyphSize = 14,
      variant = "ghost",
      onClick = function()
        heron.openUrl(eventUrl)
      end,
    })
  end
  if #buttons > 0 then
    lines[#lines + 1] = ui.row({ gap = 6, align = "center" }, buttons)
  end
  if #lines == 0 then
    lines[#lines + 1] = ui.label({ text = noctalia.tr("calendar.no_details"), fontSize = 11, color = "on_surface/0.55" })
  end

  return ui.column({
    key = "ev-detail-" .. event.id,
    gap = 6,
    paddingH = 12,
    paddingV = 8,
    radius = 6,
    fill = "surface_variant/0.35",
  }, lines)
end

local function eventRow(event, now)
  local key = "ev-" .. event.id
  local ongoing = event.start <= now and event["end"] > now
  local expanded = expandedId == event.id
  local props = {
    key = key,
    gap = 10,
    paddingH = 8,
    paddingV = 6,
    align = "center",
    radius = 6,
    onHover = "onRowHover",
    onClick = function()
      onEventClicked(event)
    end,
  }
  if hoveredKey == key or expanded then
    props.fill = "surface_variant/0.5"
  elseif ongoing then
    props.fill = "primary/0.12"
  end

  local when = heron.timeRange(event)
  if heron.dayBucket(event.start, now) == "later" then
    when = noctalia.formatTime("%a %d %b", event.start) .. "\n" .. when
  end

  local titleRow = {
    ui.label({ text = event.title, fontSize = 12, fontWeight = "bold", maxLines = 1, flexGrow = 1 }),
  }
  if event.meeting_url ~= "" then
    titleRow[#titleRow + 1] = ui.glyph({ name = "video", size = 12, color = "primary" })
  end
  local meta = {
    ui.box({ width = 8, height = 8, radius = 2, fill = event.color ~= "" and event.color or "on_surface/0.4" }),
    ui.label({ text = event.calendar, fontSize = 11, color = "on_surface/0.6", maxLines = 1 }),
  }
  if event.location ~= "" then
    meta[#meta + 1] = ui.glyph({ name = "map-pin", size = 11, color = "on_surface/0.5" })
    meta[#meta + 1] = ui.label({ text = event.location, fontSize = 11, color = "on_surface/0.6", maxLines = 1, flexGrow = 1 })
  end

  local row = ui.row(props, {
    heron.cell(104, {
      text = when,
      fontSize = 11,
      color = ongoing and "primary" or "on_surface/0.7",
      fontWeight = ongoing and "bold" or "normal",
      maxLines = 2,
    }),
    ui.column({ gap = 2, flexGrow = 1 }, {
      ui.row({ gap = 6, align = "center" }, titleRow),
      ui.row({ gap = 6, align = "center" }, meta),
    }),
    ui.glyph({ name = expanded and "chevron-up" or "chevron-down", size = 12, color = "on_surface/0.4" }),
  })
  if not expanded then
    return row
  end
  return ui.column({ key = key .. "-wrap", gap = 2 }, { row, expandedView(event) })
end

local function groupHeader(bucket)
  return ui.label({
    key = "group-" .. bucket,
    text = noctalia.tr("calendar." .. bucket),
    fontSize = 10,
    fontWeight = "bold",
    color = "on_surface/0.5",
  })
end

local function build()
  local snapshot = heron.snapshot(current)
  local now = heron.now()

  local groups = { today = {}, tomorrow = {}, later = {} }
  local total = 0
  if snapshot then
    for _, event in ipairs(snapshot.calendar) do
      if event["end"] > now then
        local bucket = heron.dayBucket(event.start, now)
        groups[bucket][#groups[bucket] + 1] = event
        total += 1
      end
    end
  end

  local summary = snapshot and noctalia.trp("calendar.summary", total) or ""
  local children = {
    heron.header("calendar-event", noctalia.tr("calendar.title"), summary, "onRefreshClicked", "onCloseClicked"),
  }
  local banner = heron.statusBanner(current)
  if banner then
    children[#children + 1] = banner
  end

  local rows = {}
  if snapshot then
    for _, row in ipairs(heron.problemRows(heron.problems(snapshot, heron.CALENDAR_KINDS))) do
      rows[#rows + 1] = row
    end
    for _, bucket in ipairs({ "today", "tomorrow", "later" }) do
      local events = groups[bucket]
      if #events > 0 or bucket == "today" then
        rows[#rows + 1] = groupHeader(bucket)
        if #events == 0 then
          rows[#rows + 1] = ui.label({
            key = "empty-today",
            text = noctalia.tr("calendar.nothing_today"),
            fontSize = 12,
            color = "on_surface/0.5",
          })
        end
        for _, event in ipairs(events) do
          rows[#rows + 1] = eventRow(event, now)
        end
      end
    end
  end

  children[#children + 1] = ui.scroll({ flexGrow = 1, gap = 4 }, rows)
  return ui.column({ flexGrow = 1, gap = 10 }, children)
end

render = function()
  if isOpen then
    renderedMinute = math.floor(heron.now() / 60)
    heron.renderPanel(build)
  end
end

function onOpen(_context)
  isOpen = true
  current = heron.read()
  hoveredKey = nil
  expandedId = nil
  copiedId = nil
  details = {}
  panel.setWantsSecondTicks(true)
  render()
end

function onClose()
  isOpen = false
  panel.setWantsSecondTicks(false)
end

function update()
  if isOpen and math.floor(heron.now() / 60) ~= renderedMinute then
    render()
  end
end

function onRowHover(state, key)
  local nextKey = (state == "true") and key or nil
  if nextKey ~= hoveredKey then
    hoveredKey = nextKey
    render()
  end
end

function onRefreshClicked()
  heron.requestReconnect()
end

function onCloseClicked()
  panel.close()
end

function onIpc(_event, _payload) end

noctalia.state.watch(heron.STATE_KEY, function(value)
  current = type(value) == "table" and value or heron.read()
  render()
end)
