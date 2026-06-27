--- Minimal class system: metatable-based classes with single inheritance.
---
--- `Class.new()` creates a class; `Class.new(parent)` (or `Class.extend()`)
--- derives one. Instances are created with `Class.new`'s generated `new(...)`,
--- which sets the metatable and, if the class defines `init`, calls
--- `self:init(...)`. The class system knows nothing about themes or formatters.

--- The shape every class object gets from `Class.new()`.
--- `T` is the instance/subclass type so that `new` and `extend` carry the
--- correct return type for each concrete class without re-declaration.
---@class TircClassDef<T, Opts>
---@field new fun(opts?: Opts): T
---@field extend fun(): T

local M = {}

--- Creates a new class, optionally inheriting from `parent`.
--- Attaches `new` and `extend` to the class; both capture it so subclasses
--- construct themselves correctly even when dot-called.
---@generic T: TircClassDef<T, Opts>, Opts
---@param parent? T
---@return T
function M.new(parent)
  local class = {}

  if parent then
    setmetatable(class, { __index = parent })
  end
  class.__index = class

  function class.new(...)
    local self = setmetatable({}, class)

    if self.init then
      self:init(...)
    end

    return self
  end

  function class.extend()
    return M.new(class)
  end

  return class
end

return M
