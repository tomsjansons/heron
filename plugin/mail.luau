--!nonstrict
-- Bar widget `mail`: "(N) new; (M) unread" across every mail account.
--
-- The glyph turns primary while anything is new, and dims while herond is not
-- reachable (the tooltip says why). Left click toggles the inbox panel.
--
--   noctalia msg plugin toms/heron:mail focused probe

local heron = require("./lib/heron.luau")

local current = heron.read()

local function render()
  local snapshot = heron.snapshot(current)
  barWidget.setGlyph("mail")

  if not snapshot then
    barWidget.setGlyphColor(heron.DIM_ROLE)
    barWidget.setText("")
    barWidget.setTooltip(heron.statusText(current))
    return
  end

  local problems = heron.problems(snapshot, heron.MAIL_KINDS)
  if snapshot.new > 0 then
    barWidget.setGlyphColor("primary")
  elseif #problems > 0 then
    barWidget.setGlyphColor("error")
  else
    barWidget.setGlyphColor("on_surface")
  end
  barWidget.setText(string.format(
    "(%d) %s; (%d) %s",
    snapshot.new, noctalia.tr("counts.new"),
    snapshot.unread, noctalia.tr("counts.unread")
  ))

  local rows = {}
  for _, account in ipairs(snapshot.mail) do
    rows[#rows + 1] = {
      key = account.id,
      value = noctalia.tr("widget.mail.account", { new = account.new, unread = account.unread }),
    }
  end
  for _, source in ipairs(problems) do
    rows[#rows + 1] = { key = source.id, value = heron.problemText(source) }
  end
  if #rows == 0 then
    barWidget.setTooltip(noctalia.tr("widget.mail.empty"))
  else
    barWidget.setTooltip(rows)
  end
end

function onClick()
  noctalia.togglePanel(heron.PANEL_INBOX)
end

function onIpc(_event, _payload) end

noctalia.state.watch(heron.STATE_KEY, function(value)
  current = type(value) == "table" and value or heron.read()
  render()
end)

render()
