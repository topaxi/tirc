---@alias EventName 'message'
---@alias FormatterName 'buffer_title' | 'message_time' | 'message_text' | 'user'

--- Styled span tree consumed by the renderer: a string, a `{ content, style }`
--- pair, or a (possibly nested) list of either. Returning `nil` skips the line.
---@alias TircSpans string | table

---@class TircSender
---@field send_privmsg fun(target: string, message: string)
---@field send_notice fun(target: string, message: string)

--- A single IRCv3 message tag as a `{ name, value }` pair.
---@alias TircMessageTag [string, string]

---@class TircMessage
---@field command string IRC verb, or symbolic name for numeric replies (e.g. 'RPL_WELCOME')
---@field params string[]
---@field nick? string set when the message carries a user prefix
---@field user? string
---@field host? string
---@field server? string set instead of nick/user/host for server prefixes
---@field tags TircMessageTag[]
---@field raw string the raw IRC line, also returned by `tostring(msg)`

---@class TircUser
---@field nickname string
---@field access_levels string[] e.g. `{ 'Owner', 'Voice' }`
---@field highest_access_level string

---@class TircDateTime
---@field year integer
---@field month integer
---@field day integer
---@field hour integer
---@field minute integer
---@field second integer

---@class TircUiFormat
---@field buffer_title? fun(server: string, nickname: string, buffer: string): TircSpans
---@field message_time? fun(date_time: TircDateTime, msg: TircMessage): TircSpans
---@field message_text? fun(msg: TircMessage, nickname: string): TircSpans?
---@field user? fun(user: TircUser): TircSpans

---@class TircUi
---@field format? TircUiFormat

---@class TircModule
---@field version string
---@field ui TircUi
---@field on fun(event_name: EventName, callback: fun(msg: table, irc: TircSender))
local M = {}

local _tirc = require('_tirc')

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

setmetatable(M, {
  __index = function(_, key)
    if key == 'ui' then
      return _tirc.__get_ui()
    end

    return _tirc[key]
  end,
  __newindex = function(t, key, value)
    if key == 'ui' then
      return _tirc.__set_ui(value)
    end

    rawset(t, key, value)
  end,
})

return M
