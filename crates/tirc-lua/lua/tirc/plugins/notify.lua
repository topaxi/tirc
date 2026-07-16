--- Desktop notifications on highlights and direct messages via `notify-send`.
---
--- Usage from `init.lua`:
---
---   tirc.use(require('tirc.plugins.notify'), {
---     urgency = 'critical',
---     patterns = { 'tirc' },
---   })
---
--- A notification fires for incoming messages that mention your nick (matched
--- on word boundaries, so `dan` does not fire on `danger`), match one of the
--- extra `patterns`, or arrive as a direct message. Notifications are
--- suppressed while the terminal is focused *and* the message's buffer is the
--- focused one.
---
--- Known limitation: Matrix direct messages are not distinguishable from
--- regular rooms in the event payload, so they only notify when they match
--- your nick or a pattern.

local tirc = require('tirc')
local process = require('tirc.process')
local utils = require('tirc.utils')

--- Options for the notify plugin.
---@class TircNotifyOptions
---@field command? string|string[] notifier program (default 'notify-send'); a list carries leading arguments, e.g. { 'flatpak-spawn', '--host', 'notify-send' }
---@field app_name? string `-a` flag (default 'tirc')
---@field urgency? 'low'|'normal'|'critical' `-u` flag (default 'normal')
---@field dms? boolean notify on direct messages (default true)
---@field patterns? string[] extra Lua patterns matched case-insensitively against the message text
---@field kinds? table<string, boolean> message kinds considered (default { text = true, action = true }; notices are excluded as server/CTCP noise)
---@field notify? fun(summary: string, body: string, event: TircEvent) executor override, replaces the notify-send invocation

--- Maximum notification body length in bytes; longer messages are truncated.
local MAX_BODY_LEN = 300

local M = {}

--- The notification decision, kept pure for testability: all runtime state
--- arrives via `ctx`.
---@param event TircEvent
---@param ctx { terminal_focused: boolean, focused_buffer: string|nil, opts: TircNotifyOptions }
---@return boolean
function M.should_notify(event, ctx)
  local opts = ctx.opts

  if event.type ~= 'message' or event.pending or event.redacted then
    return false
  end

  if not opts.kinds[event.kind] then
    return false
  end

  -- Own echoes never notify.
  if event:is_own() then
    return false
  end

  -- Suppress only when the user is actually looking at this buffer: terminal
  -- focused and the message's buffer is the focused one.
  if ctx.terminal_focused and ctx.focused_buffer == event:buffer_id() then
    return false
  end

  if opts.dms and event:is_dm() then
    return true
  end

  return event:is_mention(opts.patterns)
end

--- Default executor: runs `notify-send` (or `opts.command`) via
--- `tirc.process.spawn` - no shell involved, so message text needs no quoting
--- and cannot inject. Spawn failures (missing binary) are logged to the
--- `:debug` pane.
---@param summary string
---@param body string
---@param opts TircNotifyOptions
function M.exec_notify(summary, body, opts)
  local argv = {}
  local command = opts.command or 'notify-send'
  if type(command) == 'table' then
    for _, part in ipairs(command) do
      argv[#argv + 1] = part
    end
  else
    argv[#argv + 1] = command
  end

  argv[#argv + 1] = '-a'
  argv[#argv + 1] = opts.app_name or 'tirc'
  argv[#argv + 1] = '-u'
  argv[#argv + 1] = opts.urgency or 'normal'
  argv[#argv + 1] = '--'
  argv[#argv + 1] = summary
  argv[#argv + 1] = utils.truncate(body, MAX_BODY_LEN)

  process.spawn(argv, { capture = false }):catch(function(err)
    tirc.log.warn('notify:', err)
  end)
end

---@param opts? TircNotifyOptions
---@return TircNotifyOptions
local function normalize(opts)
  opts = opts or {}
  if opts.dms == nil then
    opts.dms = true
  end
  opts.kinds = opts.kinds or { text = true, action = true }
  return opts
end

---@param opts? TircNotifyOptions
function M:setup(opts)
  opts = normalize(opts)

  tirc.on('event', function(event)
    local ctx = {
      terminal_focused = tirc.terminal_focused,
      focused_buffer = tirc.focused_buffer,
      opts = opts,
    }

    if not M.should_notify(event, ctx) then
      return
    end

    local summary = event.sender.name .. ' (' .. event.target_name .. ')'
    if opts.notify then
      opts.notify(summary, event.body.text, event)
    else
      M.exec_notify(summary, event.body.text, opts)
    end
  end)
end

return M
