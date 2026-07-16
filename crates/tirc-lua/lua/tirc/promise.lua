--- A minimal promise library for the host's async primitives
--- (`tirc.process.spawn`, `tirc.http.fetch`) and anything user Lua wants to
--- defer. Single-threaded: all settlement happens on the UI thread, so
--- handlers attached to an already settled promise run synchronously.
---
---   http.fetch(url):next(function(res) ... end):catch(function(err) ... end)
---
--- Coroutine sugar: inside a `Promise.async` function, `p:await()` suspends
--- until the promise settles and re-raises rejections as errors:
---
---   local Promise = require('tirc.promise')
---   Promise.async(function()
---     local res = require('tirc.http').fetch(url):await()
---     require('tirc').log.info(res.status)
---   end)()
---
--- Rejections are silent unless a `:catch` (or `on_err`) handler is attached;
--- there is no unhandled-rejection detection.

local Class = require('tirc.class')

--- The async context of each `Promise.async` coroutine: the outer promise its
--- completion settles. Weak keys so finished coroutines are collectable.
---@type table<thread, TircPromise>
local async_contexts = setmetatable({}, { __mode = 'k' })

---@alias TircPromiseState 'pending' | 'fulfilled' | 'rejected'

---@class TircPromise: TircClassDef<TircPromise, fun(resolve: fun(value: any), reject: fun(err: any))>
---@field package _state TircPromiseState
---@field package _value any
---@field package _handlers fun(state: TircPromiseState, value: any)[]
local Promise = Class.new()

--- Marker consulted by `_settle` to adopt returned/resolved promises.
Promise._is_tirc_promise = true

local function is_promise(value)
  return type(value) == 'table' and value._is_tirc_promise == true
end

--- Creates a pending promise. The optional executor receives `resolve` and
--- `reject`; an error raised inside it rejects the promise.
---@param executor? fun(resolve: fun(value: any), reject: fun(err: any))
function Promise:init(executor)
  self._state = 'pending'
  self._handlers = {}

  if executor then
    local ok, err = pcall(executor, function(value)
      self:_settle('fulfilled', value)
    end, function(err)
      self:_settle('rejected', err)
    end)
    if not ok then
      self:_settle('rejected', err)
    end
  end
end

--- Settles the promise once; later calls are ignored. Fulfilling with another
--- promise adopts its eventual state instead (enables handler chaining).
---@package
---@param state TircPromiseState
---@param value any
function Promise:_settle(state, value)
  if self._state ~= 'pending' then
    return
  end

  if state == 'fulfilled' and is_promise(value) then
    value:next(function(v)
      self:_settle('fulfilled', v)
    end, function(e)
      self:_settle('rejected', e)
    end)
    return
  end

  self._state = state
  self._value = value

  local handlers = self._handlers
  self._handlers = {}
  for _, handler in ipairs(handlers) do
    handler(state, value)
  end
end

--- Attaches fulfillment/rejection handlers and returns a new promise settled
--- by their outcome: a handler's return value fulfills it (promises are
--- adopted), a raised error rejects it, and a missing handler propagates the
--- settled state unchanged.
---@param on_ok? fun(value: any): any
---@param on_err? fun(err: any): any
---@return TircPromise
function Promise:next(on_ok, on_err)
  local chained = Promise.new()

  local function run(state, value)
    local handler = state == 'fulfilled' and on_ok or on_err
    if not handler then
      chained:_settle(state, value)
      return
    end
    local ok, result = pcall(handler, value)
    if ok then
      chained:_settle('fulfilled', result)
    else
      chained:_settle('rejected', result)
    end
  end

  if self._state == 'pending' then
    self._handlers[#self._handlers + 1] = run
  else
    run(self._state, self._value)
  end

  return chained
end

--- Attaches a rejection handler; sugar for `p:next(nil, on_err)`.
---@param on_err fun(err: any): any
---@return TircPromise
function Promise:catch(on_err)
  return self:next(nil, on_err)
end

--- Runs `fn` when the promise settles either way, passing the settled state
--- through unchanged.
---@param fn fun()
---@return TircPromise
function Promise:finally(fn)
  return self:next(function(value)
    fn()
    return value
  end, function(err)
    fn()
    error(err, 0)
  end)
end

--- An already fulfilled promise (or an adopter when `value` is a promise).
---@param value any
---@return TircPromise
function Promise.resolve(value)
  local promise = Promise.new()
  promise:_settle('fulfilled', value)
  return promise
end

--- An already rejected promise.
---@param err any
---@return TircPromise
function Promise.reject(err)
  local promise = Promise.new()
  promise:_settle('rejected', err)
  return promise
end

--- Resumes an async coroutine and settles its outer promise when it errors or
--- finishes. `coroutine.resume` never raises, so coroutine errors surface as
--- rejections of the outer promise.
---@param co thread
local function step(co, ...)
  local outer = async_contexts[co]
  local ok, result = coroutine.resume(co, ...)
  if not ok then
    outer:_settle('rejected', result)
  elseif coroutine.status(co) == 'dead' then
    outer:_settle('fulfilled', result)
  end
  -- Still suspended: an `await` registered handlers that call `step` again.
end

--- Suspends the calling `Promise.async` coroutine until this promise settles,
--- returning the value or re-raising the rejection. Settled promises return
--- without suspending.
---@return any
function Promise:await()
  -- Check the context before the settled fast paths so misuse fails
  -- deterministically instead of only when the promise is still pending.
  local co = coroutine.running()
  if not co or not async_contexts[co] then
    error('await must be called inside a Promise.async function', 2)
  end

  if self._state == 'fulfilled' then
    return self._value
  end
  if self._state == 'rejected' then
    error(self._value, 0)
  end

  self:next(function(value)
    step(co, true, value)
  end, function(err)
    step(co, false, err)
  end)

  local ok, value = coroutine.yield()
  if not ok then
    error(value, 0)
  end
  return value
end

--- Wraps `fn` so `:await()` works inside it: calling the returned function
--- runs `fn` in a coroutine immediately and returns a promise of its first
--- return value; errors (including re-raised rejections) reject it.
---@param fn fun(...): any
---@return fun(...): TircPromise
function Promise.async(fn)
  return function(...)
    local outer = Promise.new()
    local co = coroutine.create(fn)
    async_contexts[co] = outer
    step(co, ...)
    return outer
  end
end

return Promise
