--- Methods available on every `TircBufferTab` table the host hands to Lua
--- (`tirc.buffers`, `render_buffer_tab`, `render_buffer_bar`). Attached as a
--- shared `__index` metatable by the host, refreshed on `:reload`.

local M = {}

--- Whether this buffer is the currently focused one.
---@param self TircBufferTab
---@return boolean
function M.is_focused(self)
  return require('tirc').focused_buffer == self.id
end

--- Whether this buffer's tab is currently under the mouse cursor.
---@param self TircBufferTab
---@return boolean
function M.is_hovered(self)
  return require('tirc').hovered_buffer == self.id
end

--- Queues focusing this buffer (applied by the host after the current
--- callback returns).
---@param self TircBufferTab
function M.focus(self)
  require('tirc').focus_buffer(self.id)
end

--- The short display label for this buffer's backend: the config metadata
--- `label` when set, otherwise the backend name.
---@param self TircBufferTab
---@return string
function M.backend_label(self)
  return (self.backend_metadata and self.backend_metadata.label)
    or self.backend_name
end

return M
