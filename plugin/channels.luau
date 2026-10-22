--!nonstrict
-- Panel entry `channels`: unread channels of the configured Discord server.
--
--   noctalia msg panel-toggle toms/heron:channels
--
-- Lists unread channels, most mentions first, each with a mention badge.
-- Clicking a channel opens it in the browser (or the Discord app, if it handles
-- discord.com links); the open button in the header opens the whole server.

local heron = require("./lib/heron.luau")

local isOpen = false
local hoveredKey = nil
local current = heron.read()

local render -- forward declaration

local function channelRow(channel)
  local key = "ch-" .. channel.id
  local props = {
    key = key,
    gap = 8,
    paddingH = 8,
    paddingV = 7,
    align = "center",
    radius = 6,
    onClick = function()
      heron.openUrl(channel.url)
    end,
    onHover = "onRowHover",
  }
  if hoveredKey == key then
    props.fill = "surface_variant/0.5"
  end

  local children = {
    ui.glyph({ name = "hash", size = 14, color = "on_surface/0.6" }),
    ui.label({
      text = channel.name,
      fontSize = 12,
      flexGrow = 1,
      maxLines = 1,
      fontWeight = channel.mentions > 0 and "bold" or "normal",
    }),
  }
  if channel.mentions > 0 then
    children[#children + 1] = ui.row({ paddingH = 7, paddingV = 1, radius = 8, fill = "primary", align = "center" }, {
      ui.label({ text = "@" .. channel.mentions, fontSize = 11, fontWeight = "bold", color = "on_primary" }),
    })
  end
  return ui.row(props, children)
end

local function build()
  local snapshot = heron.snapshot(current)
  local guild = snapshot and snapshot.discord or nil

  local title = guild and guild.name ~= "" and guild.name or noctalia.tr("channels.title")
  local summary = ""
  if guild then
    summary = noctalia.tr("channels.summary", { channels = guild.unread_channels, mentions = guild.mentions })
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
      for _, channel in ipairs(guild.channels) do
        rows[#rows + 1] = channelRow(channel)
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
