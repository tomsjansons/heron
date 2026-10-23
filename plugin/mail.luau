--!nonstrict
-- Bar widget `mail`: new/unread per mail account, using the account labels.
--
--   "proton 2/63 · work 0/55"   every account (the default)
--   "2/63"                      one account, with the `account` widget setting
--
-- The glyph turns primary while anything shown is new, red while a shown
-- account needs a login or is failing, and dims while herond is not reachable
-- (the tooltip says why). Left click toggles the inbox panel, opened on this
-- widget's account tab when it has one.
--
-- `account` is a per-instance setting ([[widget.setting]]): an account id or
-- label from herond's config; empty shows every account.
--
--   noctalia msg plugin toms/heron:mail focused probe

local heron = require("./lib/heron.luau")

local current = heron.read()

local function configuredAccount()
  local value = noctalia.getConfig("account")
  if type(value) ~= "string" then
    return ""
  end
  return heron.trim(value)
end

local function findAccount(snapshot, wanted)
  for _, account in ipairs(snapshot.mail) do
    if account.id == wanted or account.label == wanted then
      return account
    end
  end
  return nil
end

local function counts(account)
  return string.format("%d/%d", account.new, account.unread)
end

-- Problem sources for one account (every mail problem when nil).
local function problemsFor(snapshot, account)
  local problems = heron.problems(snapshot, heron.MAIL_KINDS)
  if not account then
    return problems
  end
  local out = {}
  for _, source in ipairs(problems) do
    if source.id == account.id then
      out[#out + 1] = source
    end
  end
  return out
end

local function render()
  local snapshot = heron.snapshot(current)
  barWidget.setGlyph("mail")

  if not snapshot then
    barWidget.setGlyphColor(heron.DIM_ROLE)
    barWidget.setText("")
    barWidget.setTooltip(heron.statusText(current))
    return
  end

  local wanted = configuredAccount()
  local rows = {}

  if wanted ~= "" then
    local account = findAccount(snapshot, wanted)
    if not account then
      barWidget.setGlyphColor(heron.DIM_ROLE)
      barWidget.setText("")
      barWidget.setTooltip(noctalia.tr("widget.mail.missing", { account = wanted }))
      return
    end
    local problems = problemsFor(snapshot, account)
    barWidget.setGlyphColor(account.new > 0 and "primary" or (#problems > 0 and "error" or "on_surface"))
    barWidget.setText(counts(account))
    rows[#rows + 1] = {
      key = account.label,
      value = noctalia.tr("widget.mail.account", { new = account.new, unread = account.unread }),
    }
    for _, source in ipairs(problems) do
      rows[#rows + 1] = { key = source.id, value = heron.problemText(source) }
    end
    barWidget.setTooltip(rows)
    return
  end

  local problems = problemsFor(snapshot, nil)
  local parts = {}
  for _, account in ipairs(snapshot.mail) do
    parts[#parts + 1] = account.label .. " " .. counts(account)
    rows[#rows + 1] = {
      key = account.label,
      value = noctalia.tr("widget.mail.account", { new = account.new, unread = account.unread }),
    }
  end
  for _, source in ipairs(problems) do
    rows[#rows + 1] = { key = source.id, value = heron.problemText(source) }
  end

  if snapshot.new > 0 then
    barWidget.setGlyphColor("primary")
  elseif #problems > 0 then
    barWidget.setGlyphColor("error")
  else
    barWidget.setGlyphColor("on_surface")
  end
  barWidget.setText(table.concat(parts, " · "))
  if #rows == 0 then
    barWidget.setTooltip(noctalia.tr("widget.mail.empty"))
  else
    barWidget.setTooltip(rows)
  end
end

function onClick()
  -- Open the inbox on this widget's account. The inbox reads the tab when it
  -- opens; the same key remembers the last tab picked inside the panel.
  local wanted = configuredAccount()
  local snapshot = heron.snapshot(current)
  if wanted ~= "" and snapshot then
    local account = findAccount(snapshot, wanted)
    if account then
      noctalia.state.set(heron.INBOX_TAB_KEY, account.id)
    end
  end
  noctalia.togglePanel(heron.PANEL_INBOX)
end

function onIpc(_event, _payload) end

noctalia.state.watch(heron.STATE_KEY, function(value)
  current = type(value) == "table" and value or heron.read()
  render()
end)

render()
