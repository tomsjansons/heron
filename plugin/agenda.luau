--!nonstrict
-- Bar widget `agenda`: the timed event in progress or the next one, as
-- "13:00 - Standup". All-day events are left to the calendar panel.
--
-- With nothing in the next 24 hours the glyph dims and the text clears. The
-- glyph turns primary while the event runs and in the 15 minutes before it.
-- Left click toggles the calendar panel. Re-renders every 30 s so the shown
-- event moves on when one ends.
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

  local text = noctalia.tr("widget.agenda.event", {
    time = heron.clock(event.start),
    title = ellipsize(event.title, TITLE_MAX),
  })
  barWidget.setGlyphColor((ongoing or event.start - now <= 15 * 60) and "primary" or "on_surface")
  barWidget.setText(text)

  -- Tooltip: the next few timed events.
  local rows = {}
  for _, upcoming in ipairs(snapshot.calendar) do
    if upcoming["end"] > now and not upcoming.all_day then
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
