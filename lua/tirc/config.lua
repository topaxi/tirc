---@class TircConfig
---@field servers TircConfigServer[]

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

--- A Matrix homeserver.
---@class TircMatrixServer
---@field protocol 'matrix'
---@field homeserver string base URL, e.g. 'https://matrix.org'
---@field user_id string e.g. '@me:matrix.org'
---@field password string
---@field device_id? string
---@field autojoin? string[] room ids/aliases to join on connect

local M = {}

---@return TircConfig
function M.create_config()
  return {
    servers = {},
  }
end

return M
