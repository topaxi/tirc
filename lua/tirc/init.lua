---@alias EventName 'event'
---@alias FormatterName 'buffer_title' | 'message_time' | 'message_text' | 'user'

--- Styled span tree consumed by the renderer: a string, a `{ content, style }`
--- pair, or a (possibly nested) list of either. Returning `nil` skips the line.
---@alias TircSpans string | table

--- Protocol-agnostic outgoing sender bound to one backend.
---@class TircSender
---@field send_message fun(target: string, message: string)
---@field send_notice fun(target: string, message: string)

--- A participant: a stable `id` (IRC nick / Matrix user id) plus optional mutable
--- `display` name. `name` is the display name when set, otherwise the id.
---@class TircUserRef
---@field id string
---@field display? string
---@field name string

--- A message body: plain `text` plus optional rich `html` (Matrix).
---@class TircBody
---@field text string
---@field html? string

--- A normalized chat event, as passed to the `message_text` formatter and the
--- `event` callback. `type` selects which fields are present.
---@class TircEvent
---@field type 'message' | 'edit' | 'redaction' | 'reaction' | 'membership' | 'topic' | 'rename' | 'quit' | 'server_info'
---@field backend { id: integer, protocol: 'irc' | 'matrix', name: string }
---@field target string buffer target (channel/room/nick)
---@field pending boolean optimistic local echo not yet confirmed
---@field redacted boolean
---@field sender? TircUserRef set for 'message'/'reaction'
---@field body? TircBody set for 'message'/'edit'
---@field kind? 'text' | 'action' | 'notice' message presentation
---@field who? TircUserRef set for 'membership'/'topic'/'rename'/'quit'
---@field change? 'present' | 'join' | 'part' | 'kick' | 'invite' | 'set_role'
---@field role? 'owner' | 'admin' | 'op' | 'halfop' | 'voice' | 'member'
---@field reason? string
---@field topic? string set for 'topic'
---@field new? string set for 'rename'
---@field code? string protocol classifier for 'server_info' (e.g. 'RPL_WELCOME', 'MODE')
---@field text? string set for 'server_info'
---@field raw? string wire representation escape hatch
---@field reactions? table<string, integer>

--- A buffer member for the `user` formatter.
---@class TircUser
---@field id string
---@field display? string
---@field name string
---@field nickname string alias of `name` for back-compat
---@field role 'owner' | 'admin' | 'op' | 'halfop' | 'voice' | 'member'

---@class TircDateTime
---@field year integer
---@field month integer
---@field day integer
---@field hour integer
---@field minute integer
---@field second integer

---@class TircUiFormat
---@field buffer_title? fun(server: string, nickname: string, buffer: string): TircSpans
---@field message_time? fun(date_time: TircDateTime, event: TircEvent): TircSpans
---@field message_text? fun(event: TircEvent, nickname: string): TircSpans?
---@field user? fun(user: TircUser): TircSpans

---@class TircUi
---@field format? TircUiFormat

---@class TircModule
---@field version string
---@field ui TircUi
---@field on fun(event_name: EventName, callback: fun(event: TircEvent, sender: TircSender))
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
