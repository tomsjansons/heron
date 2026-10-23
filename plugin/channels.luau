--!nonstrict
-- Panel entry `channels`: unread channels of the configured Discord server.
--
--   noctalia msg panel-toggle toms/heron:channels
--
-- Lists unread channels, most mentions first. Each channel header shows its
-- unread count ("100+" when capped) and @mentions; under it, previews of its
-- newest unread messages (at most 3: author, text, age), with messages that
-- notify you highlighted. Clicking a channel or a preview opens the channel in
-- the browser (or the Discord app, if it handles discord.com links); the button
-- at the bottom opens the whole server.

local heron = require("./lib/heron.luau")

local isOpen = false
local hoveredKey = nil
local current = heron.read()

local render -- forward declaration

local function badge(text, fill, color)
  return ui.row({ paddingH = 7, paddingV = 1, radius = 8, fill = fill, align = "center" }, {
    ui.label({ text = text, fontSize = 11, fontWeight = "bold", color = color }),
  })
end

local function hoverFill(props, key)
  if hoveredKey == key then
    props.fill = "surface_variant/0.5"
  end
  return props
end

local function channelRow(channel)
  local key = "ch-" .. channel.id
  local props = hoverFill({
    key = key,
    gap = 8,
    paddingH = 8,
    paddingV = 6,
    align = "center",
    radius = 6,
    onClick = function()
      heron.openUrl(channel.url)
    end,
    onHover = "onRowHover",
  }, key)

  local children = {
    ui.glyph({ name = "hash", size = 14, color = "on_surface/0.6" }),
    ui.label({
      text = channel.name,
      fontSize = 12,
      flexGrow = 1,
      maxLines = 1,
      fontWeight = "bold",
      color = channel.mentions > 0 and "primary" or "on_surface",
    }),
  }
  if channel.unread > 0 then
    children[#children + 1] = badge(
      noctalia.tr("channels.unread", { count = heron.countText(channel.unread, channel.more) }),
      "surface_variant",
      "on_surface"
    )
  end
  if channel.mentions > 0 then
    children[#children + 1] = badge("@" .. channel.mentions, "primary", "on_primary")
  end
  children[#children + 1] = ui.glyph({ name = "external-link", size = 12, color = "on_surface/0.4" })
  return ui.row(props, children)
end

-- One unread message under its channel: author, content, age. Messages that
-- notify you get an @ glyph and a primary tint. Clicking opens the channel.
local function previewRow(channel, message, now)
  local key = "msg-" .. channel.id .. "-" .. message.id
  local props = hoverFill({
    key = key,
    gap = 8,
    paddingH = 8,
    paddingV = 4,
    align = "start",
    radius = 6,
    onClick = function()
      heron.openUrl(channel.url)
    end,
    onHover = "onRowHover",
  }, key)
  if not props.fill and message.mentions_me then
    props.fill = "primary/0.10"
  end
  return ui.row(props, {
    ui.glyph({
      name = message.mentions_me and "at" or "message",
      size = 12,
      color = message.mentions_me and "primary" or "on_surface/0.35",
    }),
    ui.column({ gap = 1, flexGrow = 1 }, {
      ui.row({ gap = 6, align = "center" }, {
        ui.label({
          text = message.author,
          fontSize = 11,
          fontWeight = "bold",
          maxLines = 1,
          flexGrow = 1,
          color = message.mentions_me and "primary" or "on_surface/0.85",
        }),
        ui.label({
          text = message.timestamp > 0 and heron.relative(message.timestamp, now) or "",
          fontSize = 10,
          color = "on_surface/0.5",
        }),
      }),
      ui.label({
        text = message.content ~= "" and message.content or noctalia.tr("channels.no_text"),
        fontSize = 11,
        maxLines = 2,
        color = message.content ~= "" and "on_surface/0.75" or "on_surface/0.45",
      }),
    }),
  })
end

local function build()
  local snapshot = heron.snapshot(current)
  local guild = snapshot and snapshot.discord or nil

  local title = guild and guild.name ~= "" and guild.name or noctalia.tr("channels.title")
  local summary = ""
  if guild then
    summary = noctalia.tr("channels.summary", {
      messages = heron.countText(guild.unread_messages, guild.more),
      channels = guild.unread_channels,
      mentions = guild.mentions,
    })
  end

  local children = {
    heron.header("brand-discord", title, summary, "onRefreshClicked", "onCloseClicked"),
  }
  local banner = heron.statusBanner(current)
  if banner then
    children[#children + 1] = banner
  end

  local rows = {}
  if snapshot then
    for _, row in ipairs(heron.problemRows(heron.problems(snapshot, heron.DISCORD_KINDS))) do
      rows[#rows + 1] = row
    end
    if not guild then
      rows[#rows + 1] = ui.label({ key = "none", text = noctalia.tr("widget.discord.none"), fontSize = 12, color = "on_surface/0.6" })
    elseif #guild.channels == 0 then
      rows[#rows + 1] = ui.label({ key = "empty", text = noctalia.tr("channels.empty"), fontSize = 12, color = "on_surface/0.6" })
    else
      local now = heron.now()
      for index, channel in ipairs(guild.channels) do
        if index > 1 then
          rows[#rows + 1] = ui.separator({ key = "sep-" .. channel.id, thickness = 1, spacing = 4 })
        end
        rows[#rows + 1] = channelRow(channel)
        for _, message in ipairs(channel.messages) do
          rows[#rows + 1] = previewRow(channel, message, now)
        end
      end
    end
  end
  children[#children + 1] = ui.scroll({ flexGrow = 1, gap = 0 }, rows)

  if guild and guild.id ~= "" then
    children[#children + 1] = ui.button({
      text = noctalia.tr("channels.open_server"),
      glyph = "external-link",
      glyphSize = 14,
      variant = "ghost",
      onClick = function()
        heron.openUrl("https://discord.com/channels/" .. guild.id)
      end,
    })
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
  render()
end

function onClose()
  isOpen = false
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
