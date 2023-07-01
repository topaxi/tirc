---@alias EventName 'format-time' | 'format-message' | 'format-user'

---@class TircModule
---@field version string
---@field on fun(event_name: EventName, callback: function)
local M = {}

return M
