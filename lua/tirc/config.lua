---@class TircConfig
---@field servers TircConfigServer[]
---@field auto_reload_config? boolean reload config automatically when files change (default false)
---@field watch_files? string[] extra config-dir-relative paths to watch for auto-reload
---@field selection_mode? 'app' | 'native' default mouse-drag selection: 'app' selects in-app for clipboard yank, 'native' relies on the copy-mode toggle (default 'app')
---@field image_protocol? 'auto' | 'kitty' | 'sixel' | 'iterm2' terminal graphics protocol for inline images; 'auto' queries the terminal, the others force a protocol (default 'auto')
---@field link_previews? boolean fetch Open Graph metadata for links and preview title/description/thumbnail inline (default true); set false to avoid contacting linked servers
---@field quick_reactions? TircQuickReactions quick reactions offered on the selected message

--- Quick reactions shown on the selected message (message-select mode, entered
--- with `v`). The emojis are bound to number keys `1`..`9` in select mode.
---@class TircQuickReactions
---@field enabled? boolean enable message-select mode and the quick-reaction pill bar (default true)
---@field emojis? string[] ordered emoji offered as quick reactions (default a small common set)

--- A configured backend. `protocol` is required and selects the variant.
---@alias TircConfigServer TircIrcServer | TircMatrixServer

--- An IRC server.
---@class TircIrcServer
---@field protocol 'irc'
---@field enabled? boolean connect to this server on startup (default true); set to false to skip without removing the entry
---@field host string
---@field nickname string[]
---@field port? number defaults to 6697
---@field use_tls? boolean defaults to true
---@field accept_invalid_cert? boolean defaults to false
---@field realname? string
---@field autojoin? string[]
---@field metadata? table<string, any> free-form data passed back to Lua for rendering (e.g. `{ label = 'topaxi' }`)

--- A Matrix homeserver.
---@class TircMatrixServer
---@field protocol 'matrix'
---@field enabled? boolean connect to this server on startup (default true); set to false to skip without removing the entry
---@field homeserver string base URL, e.g. 'https://matrix.org'
---@field user_id string e.g. '@me:matrix.org'
---@field password string
---@field device_id? string
---@field autojoin? string[] room ids/aliases to join on connect
---@field metadata? table<string, any> free-form data passed back to Lua for rendering (e.g. `{ label = 'matrix' }`)

local M = {}

---@return TircConfig
function M.create_config()
  return {
    servers = {},
    auto_reload_config = false,
    watch_files = {},
    selection_mode = 'app',
    image_protocol = 'auto',
    link_previews = true,
    quick_reactions = {
      enabled = true,
      emojis = { '👍', '❤️', '😂', '🎉', '😢', '🔥' },
    },
  }
end

return M
