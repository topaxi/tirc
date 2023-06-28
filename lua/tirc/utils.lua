local M = {}

---@param t table
---@return string
function M.dump_table(t)
  local s = '{ '
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

return M