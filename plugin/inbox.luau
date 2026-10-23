--!nonstrict
-- Panel entry `inbox`: one tab per mail account, its unread messages, and a
-- message preview.
--
--   noctalia msg panel-toggle toms/heron:inbox
--
-- List view: a row of account tabs (label and unread count; the selected tab is
-- remembered in noctalia.state "heron.inbox.tab", which the mail widget also
-- sets), then that account's messages: a dot (primary = new, muted = unread),
-- sender, subject and age. The envelope button marks a message read or unread
-- on the server. Sources that need a login or are failing are listed above the
-- messages.
--
-- Clicking a message opens the preview view (back button returns): subject,
-- from/to/cc/date, the body as plain text (`heron message <account> <id> --json
-- --max-chars 6000`, fetched and decoded in its own runAsync callback), the
-- message's links as clickable rows, and buttons for Mark read/unread (server),
-- Mark seen/unseen (the shell's "new" flag) and Open in browser. Opening the
-- preview marks the message seen, never read.
--
-- "New" is acknowledged when the panel closes (`heron ack`), not when it opens:
-- the dots stay visible while you look, and are gone the next time. Messages
-- pinned with "Mark unseen" stay new through the ack until marked seen.

local heron = require("./lib/heron.luau")

local BODY_MAX_CHARS = 6000
local LINKS_MAX = 40
local LINK_TEXT_MAX = 90
-- Text width inside the preview's scroll area (panel width 640 minus padding
-- and scrollbar). A label only wraps when it has a width; without one a long
-- line widens the whole panel.
local TEXT_WIDTH = 580

local isOpen = false
local hoveredKey = nil
-- "account/id" -> true while an action is in flight, so buttons can show it
-- without waiting for the next snapshot.
local pending = {}
-- Messages moved to trash, hidden until the next snapshot drops them.
local trashed = {}
local current = heron.read()

-- Preview state. `preview` is nil in the list view.
--   account  = { id, label }
--   message  = copy of the snapshot message, with local unread/new flags that
--              follow our own actions (the message leaves the snapshot once read)
--   body     = decoded `heron message` reply, nil while loading
--   error    = string when the fetch failed
--   token    = matches the in-flight fetch; stale replies are dropped
local preview = nil
local fetchToken = 0

local render -- forward declaration

-- ── Helpers ──────────────────────────────────────────────────────────────────

local function selectedTab(snapshot)
  local wanted = noctalia.state.get(heron.INBOX_TAB_KEY)
  local first = nil
  for _, account in ipairs(snapshot.mail) do
    first = first or account
    if account.id == wanted then
      return account
    end
  end
  return first
end

local function selectTab(id)
  noctalia.state.set(heron.INBOX_TAB_KEY, id)
  hoveredKey = nil
  render()
end

local function keyOf(accountId, messageId)
  return accountId .. "/" .. messageId
end

-- Runs a `heron <verb> <account> <id>` action and calls onOk after success.
local function act(verb, accountId, messageId, onOk)
  local key = keyOf(accountId, messageId)
  if pending[key] then
    return
  end
  pending[key] = verb
  heron.run({ verb, accountId, messageId }, function(result)
    pending[key] = nil
    if result.exitCode == 0 and not result.timedOut and onOk then
      onOk()
    end
    render()
  end)
  render()
end

local function toggleRead(account, message)
  act(message.unread and "mark-read" or "mark-unread", account.id, message.id)
end

-- Moves a message to the account's trash and hides it right away.
local function trash(account, message, onOk)
  act("trash", account.id, message.id, function()
    trashed[keyOf(account.id, message.id)] = true
    if onOk then
      onOk()
    end
  end)
end

-- Strips carriage returns and trailing blanks and collapses runs of blank
-- lines, so a converted HTML body does not fill the panel with whitespace.
-- A plain-find line loop on purpose: gsub over a ~20 KB body exceeds a
-- callback's CPU budget.
local function tidyBody(text)
  local out = {}
  local blank = 0
  local pos = 1
  local length = #text
  while pos <= length do
    local newline = string.find(text, "\n", pos, true)
    local stop = newline and newline - 1 or length
    local last = stop
    while last >= pos do
      local byte = string.byte(text, last)
      if byte == 13 or byte == 32 or byte == 9 then
        last -= 1
      else
        break
      end
    end
    if last < pos then
      blank += 1
      if blank == 1 then
        out[#out + 1] = ""
      end
    else
      blank = 0
      out[#out + 1] = string.sub(text, pos, last)
    end
    pos = stop + 2
  end
  return heron.trim(table.concat(out, "\n"))
end

local function decodeBody(decoded)
  local links = {}
  if type(decoded.links) == "table" then
    for _, link in ipairs(decoded.links) do
      if #links >= LINKS_MAX then
        break
      end
      if type(link) == "table" then
        local url = heron.str(link.url)
        if string.match(url, "^https?://") or string.match(url, "^mailto:") then
          local text = heron.trim(heron.str(link.text))
          links[#links + 1] = { url = url, text = heron.ellipsize(text ~= "" and text or url, LINK_TEXT_MAX) }
        end
      end
    end
  end
  local text = heron.str(decoded.text)
  -- herond already cuts at --max-chars; cap bytes too in case it does not.
  if #text > BODY_MAX_CHARS * 4 then
    text = string.sub(text, 1, BODY_MAX_CHARS * 4)
  end
  return {
    from = heron.str(decoded.from),
    to = heron.strList(decoded.to),
    cc = heron.strList(decoded.cc),
    date = type(decoded.date) == "number" and decoded.date or 0,
    subject = heron.str(decoded.subject),
    text = tidyBody(text),
    links = links,
  }
end

local function fetchBody()
  if not preview then
    return
  end
  fetchToken += 1
  local token = fetchToken
  preview.body = nil
  preview.error = nil
  local args = { "message", preview.account.id, preview.message.id, "--json", "--max-chars", tostring(BODY_MAX_CHARS) }
  heron.runJson(args, function(decoded, err)
    if not preview or token ~= fetchToken then
      return
    end
    if err then
      preview.error = err
    else
      local ok, body = pcall(decodeBody, decoded)
      if ok then
        preview.body = body
      else
        preview.error = tostring(body)
      end
    end
    render()
  end)
end

local function openPreview(account, message)
  preview = {
    account = { id = account.id, label = account.label },
    message = {
      id = message.id,
      from = message.from,
      subject = message.subject,
      received = message.received,
      url = message.url,
      unread = message.unread,
      new = message.new,
    },
  }
  hoveredKey = nil
  fetchBody()
  -- Looking at a message clears its "new" dot, but leaves it unread.
  if message.new then
    local opened = preview
    act("mark-seen", account.id, message.id, function()
      opened.message.new = false
    end)
  else
    render()
  end
end

local function closePreview()
  preview = nil
  fetchToken += 1
  hoveredKey = nil
  render()
end

-- ── List view ────────────────────────────────────────────────────────────────

local function messageRow(account, message, now)
  local key = "msg-" .. keyOf(account.id, message.id)
  local props = {
    key = key,
    gap = 10,
    paddingH = 8,
    paddingV = 6,
    align = "center",
    radius = 6,
    onClick = function()
      openPreview(account, message)
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

  local sender = { text = message.from, maxWidth = 150, fontSize = 12, maxLines = 1 }
  if message.unread then
    sender.fontWeight = "bold"
  end

  local busy = pending[keyOf(account.id, message.id)] ~= nil
  return ui.row(props, {
    ui.box(dot),
    heron.cell(150, sender),
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
    ui.button({
      key = key .. "-trash",
      glyph = "trash",
      glyphSize = 14,
      variant = "ghost",
      enabled = not busy,
      tooltip = noctalia.tr("inbox.trash"),
      onClick = function()
        trash(account, message)
      end,
    }),
  })
end

local function tabsRow(snapshot, active)
  local tabs = {}
  for _, account in ipairs(snapshot.mail) do
    local selected = active ~= nil and account.id == active.id
    local text = account.label
    if account.unread > 0 then
      text = text .. "  " .. account.unread
    end
    tabs[#tabs + 1] = ui.button({
      key = "tab-" .. account.id,
      text = text,
      glyph = account.new > 0 and "point-filled" or nil,
      glyphSize = 12,
      fontSize = 12,
      variant = selected and "secondary" or "ghost",
      selected = selected,
      tooltip = noctalia.tr("inbox.tab_tooltip", {
        account = account.id,
        provider = noctalia.tr("provider." .. account.provider),
        new = account.new,
        unread = account.unread,
      }),
      onClick = function()
        selectTab(account.id)
      end,
    })
  end
  return ui.row({ gap = 4, align = "center" }, tabs)
end

local function listView(snapshot, children)
  local active = selectedTab(snapshot)
  if #snapshot.mail > 0 then
    children[#children + 1] = tabsRow(snapshot, active)
  end

  local rows = {}
  -- Problems for this account, plus sources that match no listed account
  -- (an account that never connected has no tab).
  local known = {}
  for _, account in ipairs(snapshot.mail) do
    known[account.id] = true
  end
  local problems = {}
  for _, source in ipairs(heron.problems(snapshot, heron.MAIL_KINDS)) do
    if (active and source.id == active.id) or not known[source.id] then
      problems[#problems + 1] = source
    end
  end
  for _, row in ipairs(heron.problemRows(problems)) do
    rows[#rows + 1] = row
  end

  if active then
    if #active.messages == 0 then
      rows[#rows + 1] = ui.label({
        key = "empty-" .. active.id,
        text = noctalia.tr("inbox.account_empty"),
        fontSize = 12,
        color = "on_surface/0.5",
      })
    end
    local now = heron.now()
    for _, message in ipairs(active.messages) do
      if not trashed[keyOf(active.id, message.id)] then
        rows[#rows + 1] = messageRow(active, message, now)
      end
    end
  else
    rows[#rows + 1] = ui.label({ key = "no-accounts", text = noctalia.tr("inbox.no_accounts"), fontSize = 12, color = "on_surface/0.6" })
  end

  children[#children + 1] = ui.scroll({ flexGrow = 1, gap = 0 }, rows)
end

-- ── Preview view ─────────────────────────────────────────────────────────────

local function metaLine(name, value)
  return ui.row({ gap = 8, align = "start" }, {
    ui.label({ text = name, fontSize = 11, color = "on_surface/0.5", width = 44, minWidth = 44 }),
    ui.label({ text = value, fontSize = 11, color = "on_surface/0.85", width = TEXT_WIDTH - 52, maxLines = 2 }),
  })
end

local function linkRow(index, link)
  local key = "link-" .. index
  local props = {
    key = key,
    gap = 8,
    paddingH = 8,
    paddingV = 4,
    align = "center",
    radius = 6,
    tooltip = link.url,
    onClick = function()
      heron.openUrl(link.url)
    end,
    onHover = "onRowHover",
  }
  if hoveredKey == key then
    props.fill = "surface_variant/0.5"
  end
  return ui.row(props, {
    ui.glyph({ name = string.match(link.url, "^mailto:") and "mail" or "link", size = 12, color = "primary" }),
    ui.label({ text = link.text, fontSize = 11, color = "primary", width = TEXT_WIDTH - 40, maxLines = 1 }),
  })
end

local function previewButtons()
  local account, message = preview.account, preview.message
  local busy = pending[keyOf(account.id, message.id)] ~= nil
  local buttons = {
    ui.button({
      key = "pv-read",
      text = noctalia.tr(message.unread and "inbox.mark_read" or "inbox.mark_unread"),
      glyph = message.unread and "mail-opened" or "mail",
      glyphSize = 14,
      fontSize = 12,
      variant = "outline",
      enabled = not busy,
      onClick = function()
        local verb = message.unread and "mark-read" or "mark-unread"
        act(verb, account.id, message.id, function()
          message.unread = verb == "mark-unread"
          if verb == "mark-read" then
            message.new = false
          end
        end)
      end,
    }),
    ui.button({
      key = "pv-seen",
      text = noctalia.tr(message.new and "inbox.mark_seen" or "inbox.mark_unseen"),
      glyph = message.new and "eye-check" or "eye-off",
      glyphSize = 14,
      fontSize = 12,
      variant = "outline",
      enabled = not busy,
      tooltip = noctalia.tr("inbox.seen_tooltip"),
      onClick = function()
        local verb = message.new and "mark-seen" or "mark-unseen"
        act(verb, account.id, message.id, function()
          message.new = verb == "mark-unseen"
        end)
      end,
    }),
  }
  buttons[#buttons + 1] = ui.button({
    key = "pv-trash",
    text = noctalia.tr("inbox.trash"),
    glyph = "trash",
    glyphSize = 14,
    fontSize = 12,
    variant = "outline",
    enabled = not busy,
    onClick = function()
      trash(account, message, closePreview)
    end,
  })
  if message.url ~= "" then
    buttons[#buttons + 1] = ui.button({
      key = "pv-open",
      text = noctalia.tr("inbox.open_browser"),
      glyph = "external-link",
      glyphSize = 14,
      fontSize = 12,
      variant = "ghost",
      onClick = function()
        heron.openUrl(message.url)
      end,
    })
  end
  return ui.row({ gap = 6, align = "center" }, buttons)
end

local function previewView(children)
  -- Flags are local: copied at open, then updated by our own actions (a read
  -- message drops out of the snapshot, so it cannot be followed there).
  local message = preview.message
  local body = preview.body

  children[#children + 1] = ui.row({ gap = 8, align = "center" }, {
    ui.button({
      key = "pv-back",
      text = noctalia.tr("inbox.back"),
      glyph = "arrow-left",
      glyphSize = 14,
      fontSize = 12,
      variant = "ghost",
      onClick = closePreview,
    }),
    ui.label({ text = preview.account.label, fontSize = 11, color = "on_surface/0.5", flexGrow = 1, maxLines = 1 }),
    ui.box({ width = 8, height = 8, radius = 4, fill = message.new and "primary" or (message.unread and "on_surface/0.35" or nil) }),
    ui.label({
      text = noctalia.tr(message.new and "inbox.state_new" or (message.unread and "inbox.state_unread" or "inbox.state_read")),
      fontSize = 11,
      color = "on_surface/0.6",
    }),
  })

  local subject = body and body.subject ~= "" and body.subject or message.subject
  local rows = {
    ui.label({ key = "pv-subject", text = subject ~= "" and subject or noctalia.tr("inbox.no_subject"), fontSize = 15, fontWeight = "bold", width = TEXT_WIDTH, maxLines = 3 }),
    metaLine(noctalia.tr("inbox.from"), body and body.from ~= "" and body.from or message.from),
  }
  if body and #body.to > 0 then
    rows[#rows + 1] = metaLine(noctalia.tr("inbox.to"), heron.joinList(body.to, 6))
  end
  if body and #body.cc > 0 then
    rows[#rows + 1] = metaLine(noctalia.tr("inbox.cc"), heron.joinList(body.cc, 6))
  end
  local date = body and body.date > 0 and body.date or message.received
  if date > 0 then
    rows[#rows + 1] = metaLine(noctalia.tr("inbox.date"), noctalia.formatTime("%a %d %b %Y", date) .. " " .. heron.clock(date))
  end
  rows[#rows + 1] = previewButtons()
  rows[#rows + 1] = ui.separator({ key = "pv-sep", thickness = 1, spacing = 4 })

  if preview.error then
    rows[#rows + 1] = ui.row({ key = "pv-error", gap = 8, padding = 10, radius = 8, align = "center", fill = "error/0.12" }, {
      ui.glyph({ name = "alert-triangle", size = 16, color = "error" }),
      ui.label({
        text = noctalia.tr("inbox.body_failed", { message = preview.error }),
        fontSize = 12,
        width = TEXT_WIDTH - 140,
        maxLines = 4,
      }),
      ui.button({ text = noctalia.tr("panel.retry"), variant = "ghost", fontSize = 12, onClick = fetchBody }),
    })
  elseif not body then
    rows[#rows + 1] = ui.row({ key = "pv-loading", gap = 8, padding = 10, align = "center" }, {
      ui.glyph({ name = "loader-2", size = 16, color = "on_surface/0.6" }),
      ui.label({ text = noctalia.tr("panel.loading"), fontSize = 12, color = "on_surface/0.6" }),
    })
  else
    rows[#rows + 1] = ui.label({
      key = "pv-body",
      text = body.text ~= "" and body.text or noctalia.tr("inbox.no_text"),
      fontSize = 12,
      color = body.text ~= "" and "on_surface" or "on_surface/0.5",
      width = TEXT_WIDTH,
      maxLines = 1000,
    })
    if #body.links > 0 then
      rows[#rows + 1] = ui.label({
        key = "pv-links",
        text = noctalia.tr("inbox.links", { count = #body.links }),
        fontSize = 10,
        fontWeight = "bold",
        color = "on_surface/0.5",
      })
      for index, link in ipairs(body.links) do
        rows[#rows + 1] = linkRow(index, link)
      end
    end
  end

  children[#children + 1] = ui.scroll({ key = "pv-scroll-" .. preview.message.id, flexGrow = 1, gap = 6 }, rows)
end

-- ── Panel ────────────────────────────────────────────────────────────────────

local function build()
  local snapshot = heron.snapshot(current)
  local summary = ""
  if snapshot then
    summary = noctalia.tr("inbox.summary", { new = snapshot.new, unread = snapshot.unread })
  end

  local children = {
    heron.header("mail", noctalia.tr("inbox.title"), summary, "onRefreshClicked", "onCloseClicked"),
  }

  local banner = heron.statusBanner(current)
  if banner then
    children[#children + 1] = banner
  end

  if preview then
    previewView(children)
  elseif snapshot then
    listView(snapshot, children)
  end
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
  preview = nil
  render()
end

function onClose()
  isOpen = false
  preview = nil
  fetchToken += 1
  -- Acknowledge "new" for every account once the list has been looked at.
  -- herond keeps messages pinned with mark-unseen new.
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
