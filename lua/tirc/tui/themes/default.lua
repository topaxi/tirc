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

---@param tbl table<integer, unknown>
---@param element unknown
---@return table<integer, unknown>
local function insert_every_second(tbl, element)
  local new_tbl = {}

  for _, v in ipairs(tbl) do
    table.insert(new_tbl, v)
    table.insert(new_tbl, element)
  end

  table.remove(new_tbl, #new_tbl)

  return new_tbl
end

---@param msg table
function M.format_join(msg)
  return {
    { msg.prefix.Nickname[1], blue },
    msg.command.JOIN[3] and msg.command.JOIN[3] ~= 'Unknown' and {
      { ' (',                gray },
      { msg.command.JOIN[3], blue },
      { ')',                 gray },
    } or '',
    { ' has joined ',         twhite },
    { msg.command.JOIN[1],    green },
  }
end

---@param msg table
function M.format_part(msg)
  return {
    { msg.prefix.Nickname[1], blue },
    { ' has parted ',         twhite },
    { msg.command.PART[1],    green },
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
  -- TODO: On some messages, tags is userdata.
  --       Figure our why this is the case, crate update? irc server message
  --       differences?
  if type(msg.tags) ~= 'table' then
    return false
  end

  return utils.list_find(msg.tags, function(tag)
    return tag[1] == 'time'
  end) == nil
end

---@param msg table
---@param nickname string
function M.format_privmsg(msg, nickname)
  local is_draft = message_is_draft(msg)

  ---@type string
  local message_str = msg.command.PRIVMSG[2]
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
    is_action and format_privmsg_action_nickname(msg.prefix.Nickname[1], white)
    or format_privmsg_nickname(msg.prefix.Nickname[1], blue),
    ' ',
    format_privmsg_message(message_str),
  }
end

---@param msg table
function M.format_notice(msg)
  if msg.prefix.ServerName then
    return {
      { '!' .. msg.prefix.ServerName, green },
      ' ',
      msg.command.NOTICE[2],
    }
  elseif msg.prefix.Nickname then
    return {
      '-',
      msg.prefix.Nickname[1],
      '(',
      msg.prefix.Nickname[3],
      ')- ',
      msg.command.NOTICE[2],
    }
  end
end

local function is_string(v)
  return type(v) == 'string'
end

local function is_added_mode(v)
  return type(v) == 'table' and v.Plus
end

local function is_removed_mode(v)
  return type(v) == 'table' and v.Minus
end

local function is_noprefix_mode(v)
  return type(v) == 'table' and v.NoPrefix
end

---@param t table
local function get_first_table_key(t)
  for k, _ in pairs(t) do
    return k
  end
end

---@param mode_value string|table
local function format_mode_value(mode_value)
  if is_string(mode_value) then
    return mode_value
  elseif type(mode_value) == 'table' then
    local key = get_first_table_key(mode_value)

    return key .. '(' .. mode_value[key] .. ')'
  end
end

function M.format_mode(mode)
  if mode.Plus then
    return { { '+', green }, format_mode_value(mode.Plus[1]) }
  elseif mode.Minus then
    return { { '-', red }, format_mode_value(mode.Minus[1]) }
  elseif mode.NoPrefix then
    return format_mode_value(mode.NoPrefix[1])
  end
end

local mode_type_styles = {
  UserMODE = blue,
  ChannelMODE = green,
}

local function format_modes(modes, predicate)
  return insert_every_second(
    utils.list_map(utils.list_filter(modes, predicate), M.format_mode),
    ' '
  )
end

local function format_user_or_channel_mode(msg, mode, prefix)
  local plus = format_modes(msg.command[mode][2], is_added_mode)
  local minus = format_modes(msg.command[mode][2], is_removed_mode)
  local noprefix = format_modes(msg.command[mode][2], is_noprefix_mode)

  return {
    {
      {
        prefix .. '/',
        { msg.command[mode][1], mode_type_styles[mode] },
        #plus > 0 and { ' [', plus, ']' } or '',
        #minus > 0 and { ' [', minus, ']' } or '',
        #noprefix > 0 and { ' [', noprefix, ']' } or '',
      },
      twhite,
    },
  }
end

function M.format_channel_mode(msg)
  return format_user_or_channel_mode(msg, 'ChannelMODE', 'cmode')
end

function M.format_user_mode(msg)
  return format_user_or_channel_mode(msg, 'UserMODE', 'umode')
end

function M.format_message_text(msg, nickname)
  if msg.command.JOIN then
    return M.format_join(msg)
  elseif msg.command.PART then
    return M.format_part(msg)
  elseif msg.command.PRIVMSG then
    return M.format_privmsg(msg, nickname)
  elseif msg.command.NOTICE then
    return M.format_notice(msg)
  elseif msg.command.ChannelMODE then
    return M.format_channel_mode(msg)
  elseif msg.command.UserMODE then
    return M.format_user_mode(msg)
  elseif msg.command.Response then
    if
        msg.command.Response[1] == 'RPL_NAMREPLY'
        or msg.command.Response[1] == 'RPL_ENDOFNAMES'
    then
      return nil
    end

    return utils.list_concat(server_notice_icon, {
      table.concat(msg.command.Response[2], ' ', 2),
    })
  elseif msg.command.PING or msg.command.PONG then
    return nil
  elseif msg.command.CAP then
    return utils.list_concat(server_notice_icon, {
      'Capabilities ' .. msg.command.CAP[2] .. ' ' .. msg.command.CAP[3],
    })
  end

  return tostring(msg)
end

--- @return string|nil
local function get_time_from_tags(tags)
  if not tags or type(tags) ~= 'table' then
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
