local tirc = require('tirc')
local Theme = require('tirc.tui.themes.default')

local config = tirc.create_config()

config.servers = {
  {
    protocol = 'irc',
    host = 'irc.topaxi.ch',
    nickname = { 'Rincewind', 'Twoflower' },
    port = 6697,
    use_tls = true,
    autojoin = { '#tirc' },
    -- Free-form metadata passed back to Lua for rendering. The default
    -- theme uses `label` to shorten the buffer bar in multi-server mode.
    metadata = { label = 'topaxi' },
  },
}

tirc.use(Theme)

-- Desktop notifications on highlights/DMs (requires notify-send):
-- tirc.use(require('tirc.plugins.notify'))

-- Auto-reply to DMs while :away (adds a :back command):
-- tirc.use(require('tirc.plugins.away'))

-- Deterministic per-nick colors for easier scanning:
-- tirc.use(require('tirc.plugins.nick_colors'))

return config
