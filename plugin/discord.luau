--!nonstrict
-- Bar widget `discord`: unread messages in the configured Discord server.
--
--   "12 · @0"    twelve unread messages, none notifying you
--   "12 · @3"    ... three of them notify you (mentions, reply pings,
--                @everyone, roles): glyph and text turn primary
--   "112+ · @3"  a channel has more unread than herond counts (capped at 100)
--   "#4 · @3"    herond reports no message counts: four unread channels
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

  local unread = guild.unread_messages
  local hasUnread = unread > 0 or guild.unread_channels > 0
  if guild.mentions > 0 then
    barWidget.setGlyphColor("primary")
    barWidget.setColor("primary")
  else
    barWidget.setGlyphColor(hasUnread and "on_surface" or heron.DIM_ROLE)
    barWidget.setColor("on_surface")
  end
  if hasUnread then
    -- A herond without message counts reports only channels: show "#3" then.
    local count = unread > 0 and heron.countText(unread, guild.more) or ("#" .. guild.unread_channels)
    barWidget.setText(count .. " · @" .. guild.mentions)
  else
    barWidget.setText("")
  end

  local tooltip = noctalia.tr("widget.discord.tooltip", {
    name = guild.name,
    messages = heron.countText(unread, guild.more),
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
