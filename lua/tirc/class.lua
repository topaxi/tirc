--- Tirc theme class system: build/define helpers used by TircTheme and subclasses.
---
--- A class declares a `formatters` list naming the methods that are wired as
--- direct closures on each instance. Subclasses inherit it via `__index`.

--- The shape every class object gets from `Class.new()`.
--- `T` is the instance/subclass type so that `new` and `extend` carry the
--- correct return type for each concrete class without re-declaration.
---@class TircClassDef<T, Opts>
---@field new fun(opts?: Opts): T
---@field extend fun(): T

local M = {}

--- Builds an instance of class.
---
--- For each name in `class.formatters`, creates a closure on the instance that
--- dispatches to the class method. This lets Rust call `tirc.ui.buffer_title(...)`
--- directly without a `format` sub-table, while still honouring subclass overrides.
---
--- Constructor options:
---   `palette` - passed to `make_styles`
---   any formatter name - overrides the generated closure with a plain function
---@param class table
---@param opts? table
function M.build(class, opts)
  local self = setmetatable({}, class)

  self.styles = self:make_styles(opts and opts.palette)

  for _, name in ipairs(class.formatters or {}) do
    self[name] = function(...)
      return class[name](self, ...)
    end
  end

  if opts then
    for _, name in ipairs(class.formatters or {}) do
      if type(opts[name]) == 'function' then
        self[name] = opts[name]
      end
    end
  end

  return self
end

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

  function class.new(opts)
    return M.build(class, opts)
  end

  function class.extend()
    return M.new(class)
  end

  return class
end

return M
