--- Methods available on every `TircEvent` table the host hands to Lua
--- (formatters and `tirc.on('event', ...)` handlers). Attached as a shared
--- `__index` metatable by the host, refreshed on `:reload`.

local M = {}

--- Patterns that already produced a warning, so a broken user pattern logs
--- once instead of on every message.
local warned_patterns = {}

--- Whether this event is a direct message. IRC queries target a nick;
--- channels are `#`/`&`-prefixed and Matrix rooms are `!`-prefixed. Matrix
--- DMs are not distinguishable from regular rooms in the event payload.
---@param self TircEvent
---@return boolean
function M.is_dm(self)
  return self.target:match('^[#&!]') == nil
end

--- Whether the message text mentions your own nick (case-insensitive,
--- word-boundary match) or matches one of the extra Lua `patterns`. False for
--- events without a body.
---@param self TircEvent
---@param patterns? string[]
---@return boolean
function M.is_mention(self, patterns)
  if not self.body then
    return false
  end

  local lower = self.body.text:lower()
  local nick = self.backend.nickname

  if nick ~= '' then
    -- Pattern-escape the nick, then require word boundaries on both sides via
    -- frontier patterns so `dan` does not match inside `danger`.
    local escaped = nick:lower():gsub('%W', '%%%0')
    if lower:find('%f[%w_]' .. escaped .. '%f[^%w_]') then
      return true
    end
  end

  for _, pattern in ipairs(patterns or {}) do
    local ok, found = pcall(string.find, lower, pattern)
    if ok and found then
      return true
    end
    if not ok and not warned_patterns[pattern] then
      warned_patterns[pattern] = true
      require('tirc').log.warn('is_mention: invalid pattern', pattern, found)
    end
  end

  return false
end

--- Whether this event was sent by yourself (an echo of an own message). IRC
--- nicks are case-insensitive, so the comparison is too.
---@param self TircEvent
---@return boolean
function M.is_own(self)
  local nick = self.backend.nickname
  return nick ~= ''
    and self.sender ~= nil
    and self.sender.id:lower() == nick:lower()
end

--- The opaque id of the buffer this event belongs to, matching
--- `TircBufferTab.id` and `tirc.focused_buffer`.
---@param self TircEvent
---@return string
function M.buffer_id(self)
  return self.backend.id .. ':' .. self.target
end

return M
