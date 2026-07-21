---@module 'tirc.plugins.away'
---@module 'tirc.plugins.nick_colors'
---@module 'tirc.plugins.notify'

---@class BuiltinPlugins
---@field away TircAwayPlugin
---@field nick_colors TircNickColorsPlugin
---@field notify TircNotifyPlugin
local M = {}

return setmetatable(M, {
  __index = function(t, key)
    local module = require(('tirc.plugins.%s'):format(key))
    rawset(t, key, module)
    return module
  end,
})
