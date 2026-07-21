---@class TircThemeColor

---@alias TircThemeColorValue string | TircThemeColor

---@class TircThemeStyle
---@field fg TircThemeColorValue
---@field bg TircThemeColorValue
---@field underline boolean

---@class TircThemeModule
---@field color fun(opts: { [1]: integer, [2]: integer, [3]: integer}): TircThemeColor
---@field color_from_str fun(color_str: string): TircThemeColor
---@field style fun(opts: { fg: TircThemeColorValue?, bg: TircThemeColorValue?, underline: boolean? }): TircThemeStyle
local M = {}

return M
