local M = {}

---@param t table
---@return string
function M.dump_table(t)
  local s = tostring(t) .. ' { '
  for k, v in pairs(t) do
    if type(v) == 'table' then
      s = s .. k .. ' = ' .. M.dump_table(v) .. ', '
    else
      s = s .. k .. ' = ' .. tostring(v) .. ', '
    end
  end
  return s .. '}'
end

function M.list_append(a, ...)
  for _, b in ipairs { ... } do
    for _, v in ipairs(b) do
      table.insert(a, v)
    end
  end

  return a
end

function M.list_concat(...)
  return M.list_append({}, ...)
end

---@generic T
---@param list table<integer, T>
---@param fn fun(v: T, k: integer, list: table<integer, T>): boolean
---@return table<integer, T>
function M.list_filter(list, fn)
  local result = {}

  for k, v in ipairs(list) do
    if fn(v, k, list) then
      table.insert(result, v)
    end
  end

  return result
end

function M.list_find(list, fn)
  for k, v in ipairs(list) do
    if fn(v, k, list) then
      return v
    end
  end
end

---@generic T
---@generic U
---@param list table<integer, T>
---@param fn fun(v: T, k: integer, list: table<integer, T>): U
---@return table<integer, U>
function M.list_map(list, fn)
  local result = {}
  for k, v in ipairs(list) do
    table.insert(result, fn(v, k, list))
  end
  return result
end

---@generic T
---@param list table<integer, T>
---@param fn fun(v: T, k: integer, list: table<integer, T>): table<integer, T>
---@return table<integer, T>
function M.list_flat_map(list, fn)
  local result = {}

  for k, v in ipairs(list) do
    for _, v2 in ipairs(fn(v, k, list)) do
      table.insert(result, v2)
    end
  end

  return result
end

---@param str string
---@param sep string|nil
---@return table<integer, string>
function M.split(str, sep)
  if sep == nil then
    return { str }
  end

  local tbl = {}
  for word in str:gmatch('([^' .. sep .. ']+)') do
    table.insert(tbl, word)
  end
  return tbl
end

--- Truncates `str` to at most `max` bytes without splitting a UTF-8 multibyte
--- sequence, appending an ellipsis when it was shortened. Returns `str` unchanged
--- when already short enough.
---@param str string
---@param max integer maximum byte length before the ellipsis
---@return string
function M.truncate(str, max)
  if #str <= max then
    return str
  end
  -- Back off while the next byte is a UTF-8 continuation byte (0x80-0xBF), so we
  -- never cut through the middle of a multibyte character.
  local cut = max
  while cut > 0 do
    local b = string.byte(str, cut + 1)
    if not b or b < 0x80 or b >= 0xC0 then
      break
    end
    cut = cut - 1
  end
  return string.sub(str, 1, cut) .. '…'
end

return M
