--!nonstrict
-- Panel entry `inbox`: unread mail grouped by account.
--
--   noctalia msg panel-toggle toms/heron:inbox
--
-- Each row shows a dot (primary = new, muted = unread), sender, subject and age.
-- Clicking a row opens the message in the browser; the button at the end of the
-- row marks it read or unread on the server. Sources that need a login or are
-- failing are listed above the messages.
--
-- "New" is acknowledged when the panel closes (`heron ack`), not when it opens:
-- the dots stay visible while you look, and are gone the next time.

local heron = require("./lib/heron.luau")

local isOpen = false
local hoveredKey = nil
-- "account/id" -> true while a mark-read/unread is in flight, so the button
-- can show it without waiting for the next snapshot.
local pending = {}
local current = heron.read()

local render -- forward declaration

local function toggleRead(account, message)
  local key = account.id .. "/" .. message.id
  if pending[key] then
    return
  end
  pending[key] = true
  local verb = message.unread and "mark-read" or "mark-unread"
  heron.run({ verb, account.id, message.id }, function(_result)
    pending[key] = nil
    render()
  end)
  render()
end

local function messageRow(account, message, now)
  local key = "msg-" .. account.id .. "/" .. message.id
  local props = {
    key = key,
    gap = 10,
    paddingH = 8,
    paddingV = 6,
    align = "center",
    radius = 6,
    onClick = function()
      heron.openUrl(message.url)
    end,
    onHover = "onRowHover",
  }
  if hoveredKey == key then
    props.fill = "surface_variant/0.5"
  end

  local dot = { width = 8, height = 8, radius = 4 }
  if message.new then
    dot.fill = "primary"
  elseif message.unread then
    dot.fill = "on_surface/0.35"
  end

  local sender = { text = message.from, maxWidth = 140, fontSize = 12, maxLines = 1 }
  if message.unread then
    sender.fontWeight = "bold"
  end

  local busy = pending[account.id .. "/" .. message.id] == true
  return ui.row(props, {
    ui.box(dot),
    heron.cell(140, sender),
    ui.label({
      text = message.subject,
      flexGrow = 1,
      fontSize = 12,
      maxLines = 1,
      color = message.unread and "on_surface" or "on_surface/0.55",
    }),
    ui.label({
      text = heron.relative(message.received, now),
      width = 40,
      fontSize = 11,
      color = "on_surface/0.5",
      textAlign = "end",
    }),
    ui.button({
      key = key .. "-toggle",
      glyph = message.unread and "mail-opened" or "mail",
      glyphSize = 14,
      variant = "ghost",
      enabled = not busy,
      tooltip = noctalia.tr(message.unread and "inbox.mark_read" or "inbox.mark_unread"),
      onClick = function()
        toggleRead(account, message)
      end,
    }),
  })
end

local function accountHeader(account)
  return ui.row({ key = "acct-" .. account.id, gap = 8, paddingH = 8, paddingV = 4, align = "center" }, {
    ui.label({
      text = string.upper(account.id),
      fontSize = 10,
      fontWeight = "bold",
      color = "on_surface/0.5",
    }),
    ui.label({
      text = noctalia.tr("provider." .. account.provider),
      fontSize = 10,
      color = "on_surface/0.4",
      flexGrow = 1,
    }),
    ui.label({
      text = noctalia.tr("inbox.account_counts", { new = account.new, unread = account.unread }),
      fontSize = 10,
      color = "on_surface/0.5",
    }),
  })
end

local function build()
  local snapshot = heron.snapshot(current)
  local summary = ""
  if snapshot then
    summary = string.format(
      "(%d) %s; (%d) %s",
      snapshot.new, noctalia.tr("counts.new"),
      snapshot.unread, noctalia.tr("counts.unread")
    )
  end

  local children = {
    heron.header("mail", noctalia.tr("inbox.title"), summary, "onRefreshClicked", "onCloseClicked"),
  }

  local banner = heron.statusBanner(current)
  if banner then
    children[#children + 1] = banner
  end

  local rows = {}
  if snapshot then
    for _, row in ipairs(heron.problemRows(heron.problems(snapshot, heron.MAIL_KINDS))) do
      rows[#rows + 1] = row
    end
    local now = heron.now()
    for index, account in ipairs(snapshot.mail) do
      if index > 1 then
        rows[#rows + 1] = ui.separator({ key = "sep-" .. account.id, thickness = 1, spacing = 6 })
      end
      rows[#rows + 1] = accountHeader(account)
      if #account.messages == 0 then
        rows[#rows + 1] = ui.label({
          key = "empty-" .. account.id,
          text = noctalia.tr("inbox.account_empty"),
          fontSize = 12,
          color = "on_surface/0.5",
        })
      end
      for _, message in ipairs(account.messages) do
        rows[#rows + 1] = messageRow(account, message, now)
      end
    end
    if #snapshot.mail == 0 then
      rows[#rows + 1] = ui.label({ key = "no-accounts", text = noctalia.tr("inbox.no_accounts"), fontSize = 12, color = "on_surface/0.6" })
    end
  end

  children[#children + 1] = ui.scroll({ flexGrow = 1, gap = 0 }, rows)
  return ui.column({ flexGrow = 1, gap = 10 }, children)
end

render = function()
  if isOpen then
    heron.renderPanel(build)
  end
end

function onOpen(_context)
  isOpen = true
  current = heron.read()
  hoveredKey = nil
  render()
end

function onClose()
  isOpen = false
  local snapshot = heron.snapshot(current)
  if snapshot and snapshot.new > 0 then
    heron.run({ "ack" })
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
  -- herond pushes changes on its own; "refresh" asks the service to reconnect,
  -- which also re-reads the snapshot right away.
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
