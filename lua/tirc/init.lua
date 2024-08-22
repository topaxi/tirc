---@alias EventName 'format-buffer-title' | 'message' | 'format-message-time' | 'format-message-nickname' | 'format-message-text' | 'format-user'

---@class TircSender
---@field send_privmsg fun(target: string, message: string)
---@field send_notice fun(target: string, message: string)

---@class TircModule
---@field version string
---@field on fun(event_name: EventName, callback: fun(msg: table, irc: TircSender))
local M = {}

setmetatable(M, {
  __index = require('_tirc'),
})

function M.create_config()
  return require('tirc.config').create_config()
end

---@class TircPlugin<Args>: { setup: fun(...: Args) }

---@generic Args
---@param plugin TircPlugin<Args>
---@param ... Args
function M.use(plugin, ...)
  plugin.setup(...)
end

return M
