--!nonstrict
-- Bar widget `agenda`: the ongoing or next calendar event.
--
--   "now · Standup"       a timed event is in progress
--   "14:00 Standup"       the next event within 24 hours
--   "tomorrow 09:00 ..."  the next event is after midnight
--
-- With nothing in the next 24 hours the glyph dims and the text clears. Left
-- click toggles the calendar panel. Re-renders every 30 s so "now" and the day
-- prefix stay current between snapshots.
--
--   noctalia msg plugin toms/heron:agenda focused probe

local heron = require("./lib/heron.luau")

local HORIZON = 24 * 3600
local TITLE_MAX = 28

local current = heron.read()

local function ellipsize(text, max)
  if utf8.len(text) == nil or utf8.len(text) <= max then
    return text
  end
  return string.sub(text, 1, utf8.offset(text, max) - 1) .. "…"
end

local function render()
  local snapshot = heron.snapshot(current)
  barWidget.setGlyph("calendar-event")

  if not snapshot then
    barWidget.setGlyphColor(heron.DIM_ROLE)
    barWidget.setText("")
    barWidget.setTooltip(heron.statusText(current))
    return
  end

  local now = heron.now()
  local event, ongoing = heron.nextEvent(snapshot, now, HORIZON)
  if not event then
    barWidget.setGlyphColor(heron.DIM_ROLE)
    barWidget.setText("")
    barWidget.setTooltip(noctalia.tr("widget.agenda.empty"))
    return
  end

  local title = ellipsize(event.title, TITLE_MAX)
  local text
  if ongoing then
    text = noctalia.tr("widget.agenda.now", { title = title })
  elseif heron.dayBucket(event.start, now) == "tomorrow" then
    text = noctalia.tr("widget.agenda.tomorrow", { time = heron.clock(event.start), title = title })
  else
    text = heron.clock(event.start) .. " " .. title
  end
  barWidget.setGlyphColor((ongoing or event.start - now <= 15 * 60) and "primary" or "on_surface")
  barWidget.setText(text)

  -- Tooltip: the next few events.
  local rows = {}
  for _, upcoming in ipairs(snapshot.calendar) do
    if upcoming["end"] > now then
      local when = heron.timeRange(upcoming)
      if heron.dayBucket(upcoming.start, now) ~= "today" then
        when = noctalia.formatTime("%a", upcoming.start) .. " " .. when
      end
      rows[#rows + 1] = { key = when, value = upcoming.title }
      if #rows >= 5 then
        break
      end
    end
  end
  barWidget.setTooltip(rows)
end

function update()
  render()
end

function onClick()
  noctalia.togglePanel(heron.PANEL_CALENDAR)
end

function onIpc(_event, _payload) end

noctalia.state.watch(heron.STATE_KEY, function(value)
  current = type(value) == "table" and value or heron.read()
  render()
end)

noctalia.setUpdateInterval(30000)
render()
