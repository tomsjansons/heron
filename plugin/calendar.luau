--!nonstrict
-- Panel entry `calendar`: upcoming events in Today / Tomorrow / Later groups.
--
--   noctalia msg panel-toggle toms/heron:calendar
--
-- Each row shows the time range (or "all day"), the title, the calendar name
-- next to its color swatch, and the location. Rows with a link open it in the
-- browser. Ongoing events are highlighted. While open it checks every second
-- and re-renders when the minute changes, so "ongoing" stays honest.

local heron = require("./lib/heron.luau")

local isOpen = false
local hoveredKey = nil
local renderedMinute = nil
local current = heron.read()

local render -- forward declaration

local function eventRow(event, now)
  local key = "ev-" .. event.id
  local ongoing = event.start <= now and event["end"] > now
  local props = {
    key = key,
    gap = 10,
    paddingH = 8,
    paddingV = 6,
    align = "center",
    radius = 6,
    onHover = "onRowHover",
  }
  if event.url ~= "" then
    props.onClick = function()
      heron.openUrl(event.url)
    end
  end
  if hoveredKey == key then
    props.fill = "surface_variant/0.5"
  elseif ongoing then
    props.fill = "primary/0.12"
  end

  local when = heron.timeRange(event)
  if heron.dayBucket(event.start, now) == "later" then
    when = noctalia.formatTime("%a %d %b", event.start) .. "\n" .. when
  end

  local details = {
    ui.label({ text = event.title, fontSize = 12, fontWeight = "bold", maxLines = 1 }),
  }
  local meta = {
    ui.box({ width = 8, height = 8, radius = 2, fill = event.color ~= "" and event.color or "on_surface/0.4" }),
    ui.label({ text = event.calendar, fontSize = 11, color = "on_surface/0.6", maxLines = 1 }),
  }
  if event.location ~= "" then
    meta[#meta + 1] = ui.glyph({ name = "map-pin", size = 11, color = "on_surface/0.5" })
    meta[#meta + 1] = ui.label({ text = event.location, fontSize = 11, color = "on_surface/0.6", maxLines = 1, flexGrow = 1 })
  end
  details[#details + 1] = ui.row({ gap = 6, align = "center" }, meta)

  return ui.row(props, {
    heron.cell(104, {
      text = when,
      fontSize = 11,
      color = ongoing and "primary" or "on_surface/0.7",
      fontWeight = ongoing and "bold" or "normal",
      maxLines = 2,
    }),
    ui.column({ gap = 2, flexGrow = 1 }, details),
    ui.glyph({ name = "external-link", size = 12, color = "on_surface/0.4", visible = event.url ~= "" }),
  })
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
