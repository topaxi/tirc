--- Fetch-inspired HTTP client, promise-based. Pair with `tirc.json` for JSON
--- APIs.
---
---   local http = require('tirc.http')
---   require('tirc.promise').async(function()
---     local res = http.fetch('https://example.com/api'):await()
---     if res.ok then ... end
---   end)()

local Promise = require('tirc.promise')
local _tirc = require('_tirc')

--- Options for `http.fetch`.
---@class TircFetchOpts
---@field method? string HTTP method (default 'GET')
---@field headers? table<string, string>
---@field body? string request body
---@field timeout? number seconds until the request is aborted (default 30)

--- The resolution value of an `http.fetch` promise.
---@class TircFetchResponse
---@field status integer HTTP status code
---@field ok boolean whether the status is 2xx
---@field headers table<string, string> response headers, lowercased names, duplicates comma-joined
---@field body string response body (fully buffered)

local M = {}

--- Performs an HTTP request (fetch-style). The promise resolves with a
--- `TircFetchResponse` for every HTTP response - error statuses resolve with
--- `ok = false` - and rejects only on transport failures (DNS, TLS, timeout).
---@param url string
---@param opts? TircFetchOpts
---@return TircPromise
function M.fetch(url, opts)
  return Promise.new(function(resolve, reject)
    _tirc.__fetch(url, opts, function(ok, result)
      if ok then
        resolve(result)
      else
        reject(result)
      end
    end)
  end)
end

return M
