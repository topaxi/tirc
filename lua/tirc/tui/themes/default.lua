local tirc = require('tirc')
local date_time = require('tirc.date_time')
local utils = require('tirc.utils')
local theme = require('tirc.tui.theme')

local M = {}

local white = theme.style { fg = '#ffffff' }
local twhite = theme.style { fg = 'white' } -- this is darker than gray..
local blue = theme.style { fg = 'blue' }
local green = theme.style { fg = 'green' }
local red = theme.style { fg = 'red' }
local gray = theme.style { fg = 'gray' }
local darkgray = theme.style { fg = 'darkgray' }

local server_notice_icon = {
  { { '-', { '!', white }, '-' }, blue },
  ' ',
}

---@param msg table
function M.format_join(msg)
  local realname = msg.params[3]

  return {
    { msg.nick,      blue },
    realname and realname ~= 'Unknown' and {
      { ' (',      gray },
      { realname,  blue },
      { ')',       gray },
    } or '',
    { ' has joined ', twhite },
    { msg.params[1],  green },
  }
end

---@param msg table
function M.format_part(msg)
  return {
    { msg.nick,      blue },
    { ' has parted ', twhite },
    { msg.params[1],  green },
  }
end

---@param nickname string
---@param style TircThemeStyle
local function format_privmsg_nickname(nickname, style)
  return {
    { '<',      gray },
    { nickname, style },
    { '>',      gray },
  }
end

---@param nickname string
---@param style TircThemeStyle
local function format_privmsg_action_nickname(nickname, style)
  return { { '* ', nickname }, style }
end

---@param word string
local function is_channel(word)
  return word:match('#%w+$')
end

---@param message string
local function format_privmsg_message(message)
  local spans = utils.list_flat_map(utils.split(message, '%s'), function(word)
    if is_channel(word) then
      return { { word, green }, ' ' }
    end

    return { word, ' ' }
  end)

  table.remove(spans)

  return spans
end

local function message_is_draft(msg)
  return utils.list_find(msg.tags, function(tag)
    return tag[1] == 'time'
  end) == nil
end

---@param msg table
---@param nickname string
function M.format_privmsg(msg, nickname)
  local is_draft = message_is_draft(msg)

  ---@type string
  local message_str = msg.params[2]
  local is_action = message_str:sub(1, 8) == '\001ACTION '

  if is_action then
    message_str = message_str:sub(9, -2)
  end

  if is_draft then
    return {
      is_action and format_privmsg_action_nickname(nickname, darkgray)
      or format_privmsg_nickname(nickname, darkgray),
      ' ',
      { format_privmsg_message(message_str), darkgray },
    }
  end

  return {
    is_action and format_privmsg_action_nickname(msg.nick, white)
    or format_privmsg_nickname(msg.nick, blue),
    ' ',
    format_privmsg_message(message_str),
  }
end

---@param msg table
function M.format_notice(msg)
  if msg.server then
    return {
      { '!' .. msg.server, green },
      ' ',
      msg.params[2],
    }
  elseif msg.nick then
    return {
      '-',
      msg.nick,
      '(',
      msg.host,
      ')- ',
      msg.params[2],
    }
  end
end

---@param modestring string
local function format_modestring(modestring)
  local spans = {}

  for ch in modestring:gmatch('.') do
    if ch == '+' then
      spans[#spans + 1] = { ch, green }
    elseif ch == '-' then
      spans[#spans + 1] = { ch, red }
    else
      spans[#spans + 1] = ch
    end
  end

  return spans
end

---@param msg table
function M.format_mode(msg)
  local target = msg.params[1]
  local is_channel_mode = target:match('^[#&]')
  local prefix = is_channel_mode and 'cmode' or 'umode'
  local modestring = msg.params[2] or ''

  local args = {}
  for i = 3, #msg.params do
    args[#args + 1] = msg.params[i]
  end

  local result = {
    { prefix .. '/', twhite },
    { target,        is_channel_mode and green or blue },
    ' ',
    format_modestring(modestring),
  }

  if #args > 0 then
    result[#result + 1] = ' '
    result[#result + 1] = table.concat(args, ' ')
  end

  return result
end

---@param command string
local function is_numeric_reply(command)
  return command:match('^RPL_') ~= nil or command:match('^ERR_') ~= nil
end

function M.format_message_text(msg, nickname)
  local command = msg.command

  if command == 'JOIN' then
    return M.format_join(msg)
  elseif command == 'PART' then
    return M.format_part(msg)
  elseif command == 'PRIVMSG' then
    return M.format_privmsg(msg, nickname)
  elseif command == 'NOTICE' then
    return M.format_notice(msg)
  elseif command == 'MODE' then
    return M.format_mode(msg)
  elseif is_numeric_reply(command) then
    if command == 'RPL_NAMREPLY' or command == 'RPL_ENDOFNAMES' then
      return nil
    end

    return utils.list_concat(server_notice_icon, {
      table.concat(msg.params, ' ', 2),
    })
  elseif command == 'PING' or command == 'PONG' then
    return nil
  elseif command == 'CAP' then
    return utils.list_concat(server_notice_icon, {
      'Capabilities ' .. table.concat(msg.params, ' '),
    })
  end

  return tostring(msg)
end

--- @return string|nil
local function get_time_from_tags(tags)
  if not tags then
    return
  end

  for _, tag in ipairs(tags) do
    if tag[1] == 'time' then
      return tag[2]
    end
  end
end

function M.format_buffer_title(server, nickname, buffer_name)
  return {
    { nickname,    blue },
    { '@',         twhite },
    { server,      green },
    { ' in ',      twhite },
    { buffer_name, green },
  }
end

function M.format_message_time(dt, msg)
  local time_tag = get_time_from_tags(msg.tags)

  if time_tag then
    dt = date_time.parse_from_rfc3339(time_tag)
  end

  local is_1337 = dt.hour == 13 and dt.minute == 37

  return {
    {
      string.format('%02d:%02d:%02d', dt.hour, dt.minute, dt.second),
      is_1337 and red or twhite,
    },
    { ' ▏', twhite },
  }
end

local access_level_styles = {
  Owner = { '~', red },
  Admin = { '&', red },
  Oper = { '@', red },
  HalfOp = { '%', red },
  Voice = { '+', green },
  Member = {},
}

local function format_access_level(level)
  return access_level_styles[level]
end

function M.format_user(user)
  return {
    utils.list_map(user.access_levels, format_access_level),
    { user.nickname, blue },
  }
end

---@class (exact) TircThemeDefaultOptions

---@param _config TircThemeDefaultOptions
function M.setup(_config)
  local function handle_event(callback)
    return function(...)
      local ok, result = pcall(callback, ...)

      if ok then
        return result
      else
        return { 'ERR: ' .. tostring(result), red }
      end
    end
  end

  tirc.on('format-buffer-title', handle_event(M.format_buffer_title))
  tirc.on('format-message-time', handle_event(M.format_message_time))
  tirc.on('format-message-text', handle_event(M.format_message_text))
  tirc.on('format-user', handle_event(M.format_user))
end

return M
