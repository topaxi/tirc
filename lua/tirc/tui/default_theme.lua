local tirc = require('tirc')
local date_time = require('tirc.date_time')
local utils = require('tirc.utils')
local theme = require('tirc.tui.theme')

local M = {}

local white = theme.style { fg = '#ffffff' }
local blue = theme.style { fg = 'blue' }
local green = theme.style { fg = 'green' }
local red = theme.style { fg = 'red' }
local darkgray = theme.style { fg = 'darkgray' }

local server_notice_icon = {
  { { '-', { '!', white }, '-' }, blue },
  ' ',
}

---@param msg table
local function format_join(msg)
  return {
    { msg.prefix.Nickname[1], blue },
    {
      msg.command.JOIN[3] and msg.command.JOIN[3] ~= 'Unknown' and {
        { ' (',                darkgray },
        { msg.command.JOIN[3], blue },
        { ')',                 darkgray },
      } or '',
    },
    ' has joined ',
    { msg.command.JOIN[1],    green },
  }
end

---@param msg table
local function format_part(msg)
  return {
    { msg.prefix.Nickname[1], blue },
    ' has parted ',
    { msg.command.PART[1],    green },
  }
end

---@param nickname string
---@param style TircThemeStyle
local function format_privmsg_nickname(nickname, color)
  return {
    { '<',      darkgray },
    { nickname, color },
    { '>',      darkgray },
  }
end

---@param nickname string
---@param style TircThemeStyle
local function format_privmsg_action_nickname(nickname, style)
  return { { '* ', nickname }, style }
end

---@param message string
local function format_privmsg_message(message)
  return message
end

---@param msg table
---@param nickname string
local function format_privmsg(msg, nickname)
  local is_draft = utils.list_find(msg.tags, function(tag)
    return tag[1] == 'time'
  end) == nil

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
local function format_notice(msg)
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

local function format_mode(mode)
  if mode.Plus then
    return '+' .. format_mode_value(mode.Plus[1])
  elseif mode.Minus then
    return '-' .. format_mode_value(mode.Minus[1])
  elseif mode.NoPrefix then
    return format_mode_value(mode.NoPrefix[1])
  end
end

local mode_type_colors = {
  UserMODE = blue,
  ChannelMODE = green,
}

local function format_user_or_channel_mode(msg, mode, prefix)
  local plus = utils.list_map(
    utils.list_filter(msg.command[mode][2], is_added_mode),
    format_mode
  )

  local minus = utils.list_map(
    utils.list_filter(msg.command[mode][2], is_removed_mode),
    format_mode
  )

  local noprefix = utils.list_map(
    utils.list_filter(msg.command[mode][2], is_noprefix_mode),
    format_mode
  )

  return {
    prefix .. '/',
    { msg.command[mode][1], mode_type_colors[mode] },
    #plus > 0 and { ' [', table.concat(plus, ' '), ']' } or '',
    #minus > 0 and { ' [', table.concat(minus, ' '), ']' } or '',
    #noprefix > 0 and { ' [', table.concat(noprefix, ' '), ']' } or '',
  }
end

local function format_channel_mode(msg)
  return format_user_or_channel_mode(msg, 'ChannelMODE', 'cmode')
end

local function format_user_mode(msg)
  return format_user_or_channel_mode(msg, 'UserMODE', 'umode')
end

local function format_message(msg, nickname)
  if msg.command.JOIN then
    return format_join(msg)
  elseif msg.command.PART then
    return format_part(msg)
  elseif msg.command.PRIVMSG then
    return format_privmsg(msg, nickname)
  elseif msg.command.NOTICE then
    return format_notice(msg)
  elseif msg.command.ChannelMODE then
    return format_channel_mode(msg)
  elseif msg.command.UserMODE then
    return format_user_mode(msg)
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

local function format_time(dt, msg)
  local time_tag = get_time_from_tags(msg.tags)

  if time_tag then
    dt = date_time.parse_from_rfc3339(time_tag)
  end

  local is_1337 = dt.hour == 13 and dt.minute == 37

  return {
    {
      string.format('%02d:%02d:%02d', dt.hour, dt.minute, dt.second),
      is_1337 and red or nil,
    },
    ' ▏',
  }
end

function M.setup(config)
  tirc.on('format-time', function(dt, msg)
    if config.debug then
      return nil
    end

    local ok, str = pcall(format_time, dt, msg)

    if ok then
      return str
    else
      return 'ERR in format-time: ' .. tostring(str)
    end
  end)

  tirc.on('format-message', function(msg, nickname)
    if config.debug then
      return utils.dump_table(msg)
    end

    local ok, str = pcall(format_message, msg, nickname)

    if ok then
      return str
    else
      return 'ERR in format-message: ' .. tostring(str)
    end
  end)
end

return M
