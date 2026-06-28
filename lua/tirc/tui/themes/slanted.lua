local Default = require('tirc.tui.themes.default')
local theme = require('tirc.tui.theme')
local tirc = require('tirc')

--- Slanted-separator buffer bar inspired by tmux powerline themes.
---
--- Each tab wraps its content with diagonal separators: the first tab has no
--- leading separator (content starts flush), tabs two and onwards are preceded
--- by a space on the bar background followed by the left slant. Every tab
--- closes with a right slant that returns to the bar background:
---
---   content1 ╲  ╱ content2 ╲  ╱ content3 ╲
---
--- When a buffer name is not unique across backends the backend label and room
--- name are rendered with different background colours and their own inner
--- separator.
---
--- Requires a Nerd Font (U+E0B8 / U+E0BE) and 24-bit colour support.
---
--- Usage in init.lua:
---   local Slanted = require('tirc.tui.themes.slanted')
---   tirc.use(Slanted)
---@class SlantedTheme: TircTheme
local Slanted = Default.extend()

local SEP_LEFT = '\u{E0B8}'

local BAR_BG = '#1a1a1a'
local TAB_BG = '#303030'
local TAB_BG_BACKEND = '#444444'
local FOCUSED_BG = '#005f87'
local FOCUSED_BG_BACKEND = '#0087af'
local TAB_FG = '#9e9e9e'
local FOCUSED_FG = '#ffffff'

local function tab_bg(focused)
  return focused and FOCUSED_BG or TAB_BG
end

---@param buffer TircBufferTab
local function has_unique_name(buffer)
  local count = 0
  for _, b in ipairs(tirc.buffers) do
    if b.name == buffer.name then
      count = count + 1
    end
  end
  return count <= 1
end

-- Returns the background of the first visible segment of a tab (backend label
-- when shown, otherwise the room segment).
local function tab_entry_bg(buffer, focused)
  if not has_unique_name(buffer) then
    return focused and FOCUSED_BG_BACKEND or TAB_BG_BACKEND
  end
  return tab_bg(focused)
end

---@param buffer TircBufferTab
---@param focused boolean
local function tab_spans(buffer, focused)
  local bg = tab_bg(focused)
  local fg = focused and FOCUSED_FG or TAB_FG
  local meta = buffer.backend_metadata
  local backend_label = (meta and meta.label) or buffer.backend_name

  if has_unique_name(buffer) then
    return { { ' ' .. buffer.name .. ' ', theme.style { fg = fg, bg = bg } } }
  end

  local b_bg = focused and FOCUSED_BG_BACKEND or TAB_BG_BACKEND
  return {
    { ' ' .. backend_label .. ' ', theme.style { fg = fg, bg = b_bg } },
    { SEP_LEFT, theme.style { fg = b_bg, bg = bg } },
    { ' ' .. buffer.name .. ' ', theme.style { fg = fg, bg = bg } },
  }
end

function Slanted:render_buffer_bar(buffers)
  local row = {}

  for i, buffer in ipairs(buffers) do
    local focused = tirc.is_focused_buffer(buffer)
    local bg = tab_bg(focused)

    if i > 1 then
      local entry_bg = tab_entry_bg(buffer, focused)
      row[#row + 1] = { SEP_LEFT, theme.style { fg = BAR_BG, bg = entry_bg } }
    end

    local spans = tab_spans(buffer, focused)
    for _, span in ipairs(spans) do
      row[#row + 1] = span
    end

    row[#row + 1] = { SEP_LEFT, theme.style { fg = bg, bg = BAR_BG } }
  end

  return { rows = { row }, bg = BAR_BG }
end

return Slanted
