---@class TircConfig
---@field servers TircConfigServer[]

---@class TircConfigServer
---@field host string
---@field port number
---@field use_tls boolean
---@field accept_invalid_cert boolean
---@field nickname string[]
---@field autojoin string[]

local M = {}

---@return TircConfig
function M.create_config()
  return {
    servers = {},
  }
end

return M
