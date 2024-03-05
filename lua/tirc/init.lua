---@alias EventName 'message' | 'format-time' | 'format-message' | 'format-user'

---@class TircSender
---@field send_privmsg fun(target: string, message: string)
---@field send_notice fun(target: string, message: string)

---@class TircModule
---@field version string
---@field on fun(event_name: EventName, callback: fun(msg: table, irc: TircSender))
local M = require('_tirc')

function M.create_config()
  return require('tirc.config').create_config()
end

return M
