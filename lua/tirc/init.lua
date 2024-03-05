---@alias EventName 'message' | 'format-time' | 'format-message' | 'format-user'

---@class TircModule
---@field version string
---@field create_config fun(): TircConfig
---@field on fun(event_name: EventName, callback: function)
local M = require('_tirc')

function M.create_config()
  return require('tirc.config').create_config()
end

return M
