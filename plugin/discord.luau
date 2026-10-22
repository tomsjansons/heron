--!nonstrict
-- Bar widget `discord`: unread channels in the configured Discord server.
--
--   "3"      three unread channels
--   "3 @2"   ... with two mentions (glyph turns primary)
--
-- Dims when there is nothing unread, no server is configured, or herond is not
-- reachable. Left click toggles the channels panel; right click opens the
-- server in the browser.
--
--   noctalia msg plugin toms/heron:discord focused probe

local heron = require("./lib/heron.luau")

local current = heron.read()

local function render()
  local snapshot = heron.snapshot(current)
  barWidget.setGlyph("brand-discord")

  if not snapshot then
    barWidget.setGlyphColor(heron.DIM_ROLE)
    barWidget.setText("")
    barWidget.setTooltip(heron.statusText(current))
    return
  end

  local guild = snapshot.discord
  local problems = heron.problems(snapshot, heron.DISCORD_KINDS)
  if not guild then
    barWidget.setGlyphColor(#problems > 0 and "error" or heron.DIM_ROLE)
    barWidget.setText("")
    barWidget.setTooltip(#problems > 0 and heron.problemText(problems[1]) or noctalia.tr("widget.discord.none"))
    return
  end

  local text = tostring(guild.unread_channels)
  if guild.mentions > 0 then
    text = text .. " @" .. guild.mentions
    barWidget.setGlyphColor("primary")
  elseif guild.unread_channels > 0 then
    barWidget.setGlyphColor("on_surface")
  else
    barWidget.setGlyphColor(heron.DIM_ROLE)
  end
  barWidget.setText(guild.unread_channels > 0 and text or "")

  local tooltip = noctalia.tr("widget.discord.tooltip", {
    name = guild.name,
    channels = guild.unread_channels,
    mentions = guild.mentions,
  })
  if #problems > 0 then
    tooltip = tooltip .. "\n" .. heron.problemText(problems[1])
  end
  barWidget.setTooltip(tooltip)
end

function onClick()
  noctalia.togglePanel(heron.PANEL_CHANNELS)
end

function onRightClick()
  local snapshot = heron.snapshot(current)
  if snapshot and snapshot.discord and snapshot.discord.id ~= "" then
    heron.openUrl("https://discord.com/channels/" .. snapshot.discord.id)
  end
end

function onIpc(_event, _payload) end

noctalia.state.watch(heron.STATE_KEY, function(value)
  current = type(value) == "table" and value or heron.read()
  render()
end)

render()
