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
local utils = require('tirc.utils')
local _tirc = require('_tirc')

--- Options for the notify plugin.
---@class TircNotifyOptions
---@field command? string notifier binary (default 'notify-send'); trusted config, invoked through the shell, so a wrapper with flags works
---@field app_name? string `-a` flag (default 'tirc')
---@field urgency? 'low'|'normal'|'critical' `-u` flag (default 'normal')
---@field dms? boolean notify on direct messages (default true)
---@field patterns? string[] extra Lua patterns matched case-insensitively against the message text
---@field kinds? table<string, boolean> message kinds considered (default { text = true, action = true }; notices are excluded as server/CTCP noise)
---@field notify? fun(summary: string, body: string, event: TircEvent) executor override, replaces the notify-send invocation

--- Maximum notification body length in bytes; longer messages are truncated.
local MAX_BODY_LEN = 300

local M = {}

--- Quotes `s` for a POSIX shell: wraps it in single quotes with embedded
--- single quotes escaped, making arbitrary message text inert under `sh -c`.
---@param s string
---@return string
function M.shell_quote(s)
  return "'" .. s:gsub("'", "'\\''") .. "'"
end

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

  -- IRC nicks are case-insensitive; own echoes never notify.
  local nick = event.backend.nickname
  if nick ~= '' and event.sender.id:lower() == nick:lower() then
    return false
  end

  -- Suppress only when the user is actually looking at this buffer: terminal
  -- focused and the message's buffer is the focused one.
  local buffer_id = event.backend.id .. ':' .. event.target
  if ctx.terminal_focused and ctx.focused_buffer == buffer_id then
    return false
  end

  if opts.dms and utils.is_dm(event) then
    return true
  end

  return utils.is_mention(event.body.text, nick, opts.patterns)
end

--- Default executor: shells out to `notify-send` (or `opts.command`),
--- backgrounded with output discarded so the UI thread never blocks and stray
--- output cannot corrupt the TUI.
---@param summary string
---@param body string
---@param opts TircNotifyOptions
function M.exec_notify(summary, body, opts)
  local cmd = table.concat({
    opts.command or 'notify-send',
    '-a',
    M.shell_quote(opts.app_name or 'tirc'),
    '-u',
    M.shell_quote(opts.urgency or 'normal'),
    '--',
    M.shell_quote(summary),
    M.shell_quote(utils.truncate(body, MAX_BODY_LEN)),
  }, ' ')
  os.execute(cmd .. ' >/dev/null 2>&1 &')
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
      terminal_focused = _tirc.terminal_focused,
      focused_buffer = _tirc.focused_buffer,
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
