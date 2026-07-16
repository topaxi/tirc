local Class = require('tirc.class')

--- Builder for one buffer-bar row that keeps the row's spans and its parallel
--- `ids` hit declaration (see `TircBufferBar`) in lockstep, so a layout can
--- never skew click targets by appending to one array and forgetting the
--- other.
---
--- ```lua
--- local row = BarRow.new()
--- for _, buffer in ipairs(buffers) do
---   row:add(self:render_buffer_tab(buffer, row:first()), buffer.id)
--- end
--- return BarRow.bar(row)
--- ```
---@class TircBarRow: TircClassDef<TircBarRow, nil>
---@field spans TircSpans[] top-level row elements, one per `ids` entry
---@field ids string[] hit declaration parallel to `spans`
local BarRow = Class.new()

function BarRow:init()
  self.spans = {}
  self.ids = {}
end

--- Appends one top-level row element together with its hit id. `id` follows
--- the `TircBufferBar.ids` contract (buffer id, `backend:<id>`, ...); it
--- defaults to `''` (decoration, no click action).
---@param spans TircSpans
---@param id? string
---@return TircBarRow self for chaining
function BarRow:add(spans, id)
  self.spans[#self.spans + 1] = spans
  self.ids[#self.ids + 1] = id or ''
  return self
end

--- True while the row is still empty - i.e. the next `add` appends the row's
--- first element. Feeds the `first` parameter of
--- `render_buffer_tab`/`render_backend_tab`.
function BarRow:first()
  return #self.spans == 0
end

--- Assembles rows into the `TircBufferBar` shape consumed by the renderer,
--- keeping `rows` and `ids` paired per row.
---@param ... TircBarRow one entry per rendered bar line, in order
---@return TircBufferBar
function BarRow.bar(...)
  local rows, ids = {}, {}
  for i = 1, select('#', ...) do
    local row = select(i, ...)
    rows[#rows + 1] = row.spans
    ids[#ids + 1] = row.ids
  end
  return { rows = rows, ids = ids }
end

return BarRow
