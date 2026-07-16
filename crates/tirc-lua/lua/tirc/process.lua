--- Process spawning without a shell, promise-based.
---
---   local process = require('tirc.process')
---   process.spawn({ 'notify-send', '--', summary, body }):catch(function(err)
---     require('tirc').log.warn(err)
---   end)

local Promise = require('tirc.promise')
local _tirc = require('_tirc')

--- Options for `process.spawn`.
---@class TircSpawnOpts
---@field cwd? string working directory
---@field env? table<string, string> extra environment variables
---@field capture? boolean collect stdout/stderr (default true); false runs with null stdio and resolves with code/signal only

--- The resolution value of a `process.spawn` promise.
---@class TircSpawnResult
---@field code? integer exit code, nil when terminated by a signal
---@field signal? integer terminating signal (unix)
---@field stdout string captured stdout (empty when capture = false)
---@field stderr string captured stderr (empty when capture = false)

local M = {}

--- Spawns a process without a shell: `argv` is passed to exec verbatim, so no
--- quoting is ever needed (or possible). stdin is null and the child cannot
--- write to the terminal. Returns a promise that resolves with a
--- `TircSpawnResult` when the process exits (also on non-zero exits) and
--- rejects when it fails to run (missing binary, bad argv). Output is fully
--- buffered - not suited to long-running/chatty processes; there is no kill
--- or timeout handle. A promise pending across `:reload` settles with its
--- pre-reload handlers.
---@param argv string[] program and arguments; argv[1] is the executable
---@param opts? TircSpawnOpts
---@return TircPromise
function M.spawn(argv, opts)
  return Promise.new(function(resolve, reject)
    _tirc.__spawn(argv, opts, function(ok, result)
      if ok then
        resolve(result)
      else
        reject(result)
      end
    end)
  end)
end

return M
