--- Deterministic per-nick colors: hashes the stable user id into a curated
--- palette so the same person always renders in the same color, in message
--- lines and the userlist alike. Themes opt in via `tirc.nick_style` (the
--- bundled themes do), so the plugin works with any theme that consults the
--- hook and is invisible to those that do not.
---
--- Usage from `init.lua`:
---
---   tirc.use(require('tirc.plugins.nick_colors'))
---   -- or with a custom palette:
---   tirc.use(require('tirc.plugins.nick_colors'), {
---     palette = { '#e06c75', '#98c379', '#61afef' },
---   })

local tirc = require('tirc')
local theme = require('tirc.tui.theme')
local hash = require('tirc.hash')

--- Options for the nick_colors plugin.
---@class TircNickColorsOptions
---@field palette? string[] colours (hex or named) nicks are hashed into

--- Hand-picked hues readable on dark backgrounds.
local DEFAULT_PALETTE = {
  '#e06c75',
  '#d19a66',
  '#e5c07b',
  '#98c379',
  '#56b6c2',
  '#61afef',
  '#c678dd',
  '#f47fb4',
  '#ef7c5c',
  '#b5bd68',
  '#5fd7af',
  '#5fafff',
  '#af87ff',
  '#ff87af',
}

---@class TircNickColorsPlugin
local M = {}

M.default_palette = DEFAULT_PALETTE

--- Palette colour for a user id. Ids hash case-insensitively so IRC nick
--- casing differences map to the same colour. Pure for testability.
---@param id string
---@param palette string[]
---@return string
function M.color_for(id, palette)
  return palette[hash.xxh3(id:lower()) % #palette + 1]
end

---@param opts? TircNickColorsOptions
function M:setup(opts)
  opts = opts or {}
  local palette = opts.palette or DEFAULT_PALETTE
  local styles = {}

  tirc.set_nick_style(function(user)
    local color = M.color_for(user.id, palette)
    local style = styles[color]
    if not style then
      style = theme.style { fg = color }
      styles[color] = style
    end
    return style
  end)
end

return M
