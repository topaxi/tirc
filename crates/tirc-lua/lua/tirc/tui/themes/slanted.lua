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
--- Only the tab rendering is overridden, so every `buffer_bar` layout of the
--- default theme (and the runtime `:barstyle` switch) works with this look.
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
local MENTION_BG = '#5f1f1f'
local HOVER_BG = '#4a4a4a'
local TAB_FG = '#9e9e9e'
local FOCUSED_FG = '#ffffff'
local UNREAD_FG = '#e0e0e0'

---@param buffer TircBufferTab
---@param focused boolean
---@param hovered boolean
local function tab_bg(buffer, focused, hovered)
  if hovered then
    return HOVER_BG
  end
  if focused then
    return FOCUSED_BG
  end
  if buffer.has_mention then
    return MENTION_BG
  end
  return TAB_BG
end

---@param buffer TircBufferTab
---@param focused boolean
---@param hovered boolean
local function tab_fg(buffer, focused, hovered)
  if hovered or focused then
    return FOCUSED_FG
  end
  if buffer.has_unread then
    return UNREAD_FG
  end
  return TAB_FG
end

-- Returns the background of the first visible segment of a tab (backend label
-- when shown, otherwise the room segment).
local function tab_entry_bg(buffer, focused, show_backend, hovered)
  if hovered then
    return HOVER_BG
  end
  if show_backend then
    return focused and FOCUSED_BG_BACKEND or TAB_BG_BACKEND
  end
  return tab_bg(buffer, focused, hovered)
end

---@param buffer TircBufferTab
---@param focused boolean
---@param show_backend boolean
---@param suffix string status/latency suffix appended to the room name segment
---@param hovered boolean
local function tab_spans(buffer, focused, show_backend, suffix, hovered)
  local bg = tab_bg(buffer, focused, hovered)
  local fg = tab_fg(buffer, focused, hovered)
  local label = buffer.name .. suffix

  if not show_backend then
    return { { ' ' .. label .. ' ', theme.style { fg = fg, bg = bg } } }
  end

  local b_bg = hovered and HOVER_BG
    or (focused and FOCUSED_BG_BACKEND or TAB_BG_BACKEND)
  return {
    {
      ' ' .. buffer:backend_label() .. ' ',
      theme.style { fg = fg, bg = b_bg },
    },
    { SEP_LEFT, theme.style { fg = b_bg, bg = bg } },
    { ' ' .. label .. ' ', theme.style { fg = fg, bg = bg } },
  }
end

function Slanted:bar_background()
  return BAR_BG
end

--- Renders one buffer tab with its slant separators. The leading separator is
--- skipped for a row's first tab so content starts flush; each tab groups its
--- separators into one element (the renderer measures top-level row elements
--- for click hit-testing, so a tab's separators must live inside its own
--- element rather than being flattened into the row).
---@param buffer TircBufferTab
---@param first? boolean
function Slanted:render_buffer_tab(buffer, first)
  local focused = buffer:is_focused()
  local hovered = buffer:is_hovered()
  local show_backend = self:tab_needs_backend_prefix(buffer)
  local bg = tab_bg(buffer, focused, hovered)
  local tab = {}

  if not first then
    local entry_bg = tab_entry_bg(buffer, focused, show_backend, hovered)
    tab[#tab + 1] = { SEP_LEFT, theme.style { fg = BAR_BG, bg = entry_bg } }
  end

  local suffix = self:tab_status_suffix(buffer)
  for _, span in
    ipairs(tab_spans(buffer, focused, show_backend, suffix, hovered))
  do
    tab[#tab + 1] = span
  end

  tab[#tab + 1] = { SEP_LEFT, theme.style { fg = bg, bg = BAR_BG } }

  return tab
end

--- Renders one backend tab (the tabbed layout's first row) in the same
--- slanted look; the selected backend uses the focused colours.
---@param group { id: integer, label: string, has_unread: boolean, has_mention: boolean }
---@param first? boolean
function Slanted:render_backend_tab(group, first)
  local selected = tirc.selected_backend == group.id
  local bg = selected and FOCUSED_BG
    or (group.has_mention and MENTION_BG)
    or TAB_BG
  local fg = selected and FOCUSED_FG
    or (group.has_unread and UNREAD_FG)
    or TAB_FG
  local tab = {}

  if not first then
    tab[#tab + 1] = { SEP_LEFT, theme.style { fg = BAR_BG, bg = bg } }
  end
  tab[#tab + 1] =
    { ' ' .. group.label .. ' ', theme.style { fg = fg, bg = bg } }
  tab[#tab + 1] = { SEP_LEFT, theme.style { fg = bg, bg = BAR_BG } }

  return tab
end

return Slanted
