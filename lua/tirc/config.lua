---@class TircConfig
---@field servers TircConfigServer[]
---@field auto_reload_config? boolean reload config automatically when files change (default false)
---@field watch_files? string[] extra config-dir-relative paths to watch for auto-reload
---@field selection_mode? 'app' | 'native' default mouse-drag selection: 'app' selects in-app for clipboard yank, 'native' relies on the copy-mode toggle (default 'app')

--- A configured backend. `protocol` is required and selects the variant.
---@alias TircConfigServer TircIrcServer | TircMatrixServer

--- An IRC server.
---@class TircIrcServer
---@field protocol 'irc'
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
  }
end

return M
