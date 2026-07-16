---@alias EventName 'event' | 'away'
---@alias FormatterName 'buffer_title' | 'userlist_title' | 'message_time' | 'message_text' | 'link_preview' | 'user' | 'render_buffer_tab'

--- A buffer entry passed to the `render_buffer_tab` formatter.
---@class TircBufferTab
---@field id string opaque buffer identifier (matches `event:buffer_id()` and `tirc.focused_buffer`)
---@field name string display name (may differ from target for Matrix rooms)
---@field target string raw target identifier (IRC channel/nick or Matrix room id)
---@field backend_id integer id of the backend this buffer belongs to (for grouping)
---@field backend_name string human-readable backend name
---@field backend_metadata? table<string, any> per-server metadata from the config (e.g. `{ label = 'topaxi' }`)
---@field has_unread boolean true when unseen messages are present
---@field has_mention boolean true when the user's nick was mentioned in an unseen message
---@field is_status boolean true for the backend's status/server buffer
---@field is_system boolean true for a homeserver system buffer (e.g. a Matrix server-notices room)
---@field is_focused fun(self: TircBufferTab): boolean whether this buffer is the currently focused one
---@field focus fun(self: TircBufferTab) queue focusing this buffer (applied after the current callback)
---@field backend_label fun(self: TircBufferTab): string the backend's `metadata.label`, falling back to its name

--- The buffer bar layout returned by `render_buffer_bar`: one `TircSpans` per row.
---@class TircBufferBar
---@field rows TircSpans[]
---@field ids? string[][] hit-region declaration parallel to `rows`: `ids[r][e]` describes the e-th top-level element of `rows[r]`. Entries: a buffer id (`TircBufferTab.id`, click focuses it), `'backend:<id>'` (click focuses that backend's last-viewed buffer), `'backend-select:<id>'` (click only selects the backend's row), `'custom:<anything>'` (click calls the theme's `on_bar_click` with the id, for custom UX), or `''` for decoration. Use `''`, never nil - sequence holes truncate. Absent: first-row elements map to buffers in order (legacy)
---@field anchors? integer[] per-row 1-based index of the element each row scrolls to keep visible, overriding the automatic focused-buffer/selected-backend anchor; 0 or absent entries fall back to automatic
---@field bg? string optional base background colour (hex or named) to fill empty bar space
---@field scroll? 'follow'|'center' how to scroll each bar row to keep its anchor tab visible: 'follow' (default) scrolls minimally; 'center' always centers

--- Styled span tree consumed by the renderer: a string, a `{ content, style }`
--- pair, or a (possibly nested) list of either. Returning `nil` skips the line.
---@alias TircSpans string | table

--- Protocol-agnostic outgoing sender bound to one backend.
---@class TircSender
---@field send_message fun(target: string, message: string)
---@field send_notice fun(target: string, message: string)

--- A participant: a stable `id` (IRC nick / Matrix user id) plus optional mutable
--- `display` name. `name` is the display name when set, otherwise the id.
---@class TircUserRef
---@field id string
---@field display? string
---@field name string

--- A media attachment on a message (image, file, ...). `kind` is one of
--- 'image' | 'video' | 'audio' | 'file'; `url` is a resolvable link when known.
---@class TircAttachment
---@field kind 'image' | 'video' | 'audio' | 'file'
---@field name string
---@field url? string
---@field mime? string

--- A message body: plain `text` plus optional rich `html` (Matrix) and media
--- `attachments`.
---@class TircBody
---@field text string
---@field html? string
---@field attachments? TircAttachment[]

--- A normalized chat event, as passed to the `message_text` formatter and the
--- `event` callback. `type` selects which fields are present.
---@class TircEvent
---@field type 'message' | 'edit' | 'redaction' | 'reaction' | 'membership' | 'topic' | 'rename' | 'quit' | 'server_info'
---@field backend { id: integer, protocol: 'irc' | 'matrix' | 'mattermost', name: string, nickname: string, metadata?: table<string, any> } `nickname` is your own nick/user id on this backend, rename-aware
---@field target string buffer target (channel/room/nick)
---@field target_name string friendly buffer name (Matrix room name); equals `target` for IRC
---@field pending boolean optimistic local echo not yet confirmed
---@field redacted boolean
---@field edited boolean
---@field sender? TircUserRef set for 'message'/'reaction'
---@field body? TircBody set for 'message'/'edit'
---@field kind? 'text' | 'action' | 'notice' message presentation
---@field who? TircUserRef set for 'membership'/'topic'/'rename'/'quit'
---@field change? 'present' | 'join' | 'part' | 'kick' | 'invite' | 'set_role'
---@field realname? string IRC extended-join real name, set for 'join'
---@field role? 'owner' | 'admin' | 'op' | 'halfop' | 'voice' | 'member'
---@field reason? string
---@field topic? string set for 'topic'
---@field new? string set for 'rename'
---@field from? string originating server/nick for 'server_info'
---@field code? string protocol classifier for 'server_info' (e.g. 'RPL_WELCOME', 'MODE')
---@field text? string set for 'server_info'
---@field raw? string wire representation escape hatch
---@field reactions? table<string, TircReaction> emoji key -> aggregated state
---@field is_dm fun(self: TircEvent): boolean whether this is a direct message (Matrix DMs are indistinguishable from rooms)
---@field is_mention fun(self: TircEvent, patterns?: string[]): boolean whether the text mentions your own nick (word-boundary) or matches one of `patterns`
---@field is_own fun(self: TircEvent): boolean whether you sent this event (echo of an own message)
---@field buffer_id fun(self: TircEvent): string the opaque id of this event's buffer, matching `TircBufferTab.id`/`tirc.focused_buffer`

--- Aggregated state for one reaction key on a message.
---@class TircReaction
---@field count integer number of people who reacted with this key
---@field mine boolean whether the local user is one of them

--- A fetched Open Graph link preview passed to the `link_preview` formatter.
--- Any field except `url` may be absent.
---@class TircLinkPreview
---@field url string the previewed link
---@field title? string og:title (or the page <title>)
---@field description? string og:description, truncated for display
---@field site_name? string og:site_name (e.g. 'YouTube')

--- One clickable reaction pill returned by the `render_reactions` formatter.
---@class TircReactionPill
---@field key string emoji key this pill toggles
---@field spans TircSpans styled content drawn for the pill

--- A buffer member for the `user` formatter.
---@class TircUser
---@field id string
---@field display? string
---@field name string
---@field nickname string alias of `name` for back-compat
---@field role 'owner' | 'admin' | 'op' | 'halfop' | 'voice' | 'member'

--- A calendar date-time. Supports `tostring(dt)` and strftime-style
--- formatting via `dt:format('%H:%M')`.
---@class TircDateTime
---@field year integer
---@field month integer
---@field day integer
---@field hour integer
---@field minute integer
---@field second integer
---@field format fun(self: TircDateTime, fmt: string): string strftime-style formatting (chrono syntax)

---@class TircUi
---@field buffer_title? fun(server: string, nickname: string, buffer: string): TircSpans
---@field userlist_title? fun(buffer: string): TircSpans
---@field message_time? fun(date_time: TircDateTime, event: TircEvent): TircSpans
---@field message_text? fun(event: TircEvent, nickname: string): TircSpans?
---@field link_preview? fun(preview: TircLinkPreview): TircSpans[]
---@field render_reactions? fun(event: TircEvent, hovered_key: string|nil): TircReactionPill[]
---@field render_quick_reactions? fun(event: TircEvent, emojis: string[], hovered_key: string|nil): TircReactionPill[]
---@field user? fun(user: TircUser): TircSpans
---@field render_buffer_tab? fun(buffer: TircBufferTab, first?: boolean): TircSpans
---@field render_buffer_bar? fun(buffers: TircBufferTab[]): TircBufferBar | TircSpans
---@field render_unread_separator? fun(width: integer): TircSpans
---@field render_date_separator? fun(date: TircDateTime, width: integer): TircSpans
---@field buffer_bar_styles? string[] bar layout names the theme understands, surfaced by `:barstyle`
---@field on_bar_click? fun(id: string) handler for clicks on `'custom:<...>'` bar elements

--- What part of the input activates a completion source. `sigil` opens on a
--- character typed at the start of a word (e.g. `:` for emoji, `@` for
--- mentions); `line_start` completes the first word of the line.
---@class TircCompletionTrigger
---@field kind 'sigil' | 'line_start'
---@field char? string the sigil character, required for kind 'sigil'
---@field min_chars? integer minimum query length before suggesting (default 1)

--- The context passed to a completion source's `complete` function.
---@class TircCompletionContext
---@field input string the full input line
---@field cursor integer cursor position as a 0-based character index
---@field query string the text between the trigger and the cursor
---@field mode 'insert' | 'command'

--- One completion suggestion. `insert` replaces the trigger span (including
--- the sigil); `label` is what the popup shows and defaults to `insert`. A
--- plain string is shorthand for both.
---@class TircCompletionItem
---@field insert string
---@field label? string

--- A Lua completion source: a declarative trigger plus a `complete` function
--- returning items for the extracted query. Consulted after the builtin
--- sources (command names, emoji); cleared and re-registered on `:reload`.
---@class TircCompletionSource
---@field name? string used in error logs
---@field mode 'insert' | 'command'
---@field trigger TircCompletionTrigger
---@field complete fun(ctx: TircCompletionContext): (TircCompletionItem | string)[]

--- The context passed to a user command's handler.
---@class TircCommandContext
---@field name string the resolved command name
---@field args string everything after the command name, unsplit
---@field fargs string[] `args` split on whitespace
---@field buffer? string target of the focused buffer, or nil
---@field backend? integer id of the focused buffer's backend, or nil

--- The context passed to a user command's `complete` function.
---@class TircCommandCompleteContext
---@field input string the full input line
---@field cursor integer cursor position as a 0-based character index
---@field query string the argument word text before the cursor
---@field arg_index integer 1-based argument position, matching `fargs`
---@field args string everything after the command name

--- Options for `tirc.create_command`, nvim_create_user_command-style.
---@class TircCommandOpts
---@field nargs? '0' | '1' | '?' | '*' | '+' | 0 | 1 argument arity (default '0')
---@field complete? 'channel' | 'nick' | 'buffer' | fun(ctx: TircCommandCompleteContext): (TircCompletionItem | string)[]
---@field desc? string

---@class TircModule
---@field version string
---@field ui TircUi
---@field focused_buffer? string opaque id of the currently focused buffer, or nil
---@field selected_backend? integer id of the backend a tabbed buffer bar shows (falls back to the focused buffer's backend), or nil
---@field buffer_bar_style? string runtime `:barstyle` override for the buffer-bar layout, or nil when the theme option applies
---@field mode 'normal' | 'command' | 'insert' | 'select' current editor mode
---@field multi_backend boolean whether more than one backend is connected
---@field terminal_focused boolean whether the terminal window has focus (true when the terminal does not report focus events)
---@field buffers TircBufferTab[] all open buffers
---@field focus_buffer fun(id: string) queue focusing a buffer by its opaque id (applied after the current callback)
---@field select_backend fun(backend_id: integer) queue selecting a backend for the tabbed bar (applied after the current callback)
---@field set_away fun(message: string|nil) queue setting (message) or clearing (nil) the away state on all backends (applied after the current callback)
---@field on fun(event_name: 'event', callback: fun(event: TircEvent, sender: TircSender)) | fun(event_name: 'away', callback: fun(message: string|nil))
---@field register_completion_source fun(source: TircCompletionSource)
---@field create_command fun(name: string, handler: fun(ctx: TircCommandContext, sender: TircSender|nil), opts?: TircCommandOpts) register a `:` user command; builtin names shadow it on exact match; cleared and re-registered on `:reload`
---@field nick_style fun(user: TircUserRef|TircUser): TircThemeStyle|nil per-nick style from the registered provider, or nil (themes fall back to their own style)
---@field set_nick_style fun(provider: (fun(user: TircUserRef|TircUser): TircThemeStyle|nil)|nil) register the nick-style provider; a single slot, cleared on `:reload`
---@field log TircLog logging helpers that write to the `:debug` pane
local M = {}

local _tirc = require('_tirc')

---@return TircConfig
function M.create_config()
  return require('tirc.config').create_config()
end

---@class TircPlugin<Args>: { setup: fun(self: TircPlugin, ...: Args) }

--- Calls the plugin's `setup` method-style, passing the plugin itself as the
--- receiver. This lets a subclass (e.g. a theme created via `extend`) construct
--- itself rather than the base class whose `setup` it inherited.
---@generic Args
---@param plugin TircPlugin<Args>
---@param ... Args
function M.use(plugin, ...)
  plugin:setup(...)
end

--- Queues a UI action for the host to apply after the current callback
--- returns. Used by the `tirc.focus_buffer`/`tirc.select_backend` helpers so
--- theme handlers (e.g. `on_bar_click`) can drive the UI.
---@param action table
local function queue_ui_action(action)
  local actions = _tirc.__ui_actions
  if not actions then
    actions = {}
    _tirc.__ui_actions = actions
  end
  actions[#actions + 1] = action
end

--- Focuses a buffer by its opaque id (`TircBufferTab.id`). Applied by the host
--- after the current callback returns; unknown ids are ignored.
---@param id string
function M.focus_buffer(id)
  queue_ui_action { type = 'focus_buffer', id = id }
end

--- Selects the backend whose buffers a tabbed buffer bar shows, without moving
--- focus. Applied by the host after the current callback returns.
---@param backend_id integer
function M.select_backend(backend_id)
  queue_ui_action { type = 'select_backend', id = backend_id }
end

--- Sets or clears the away state on every backend (native away where the
--- protocol supports it) and fires the `away` event. Applied by the host after
--- the current callback returns. Pass a message to go away, nil to come back.
---@param message string|nil
function M.set_away(message)
  queue_ui_action { type = 'set_away', message = message }
end

--- Resolves the per-nick style from the registered provider, or nil when no
--- provider is set. Themes consult this wherever they style a nick and fall
--- back to their own style on nil, so plugins (e.g.
--- `tirc.plugins.nick_colors`) can recolor nicks transparently.
---@param user TircUserRef|TircUser
---@return TircThemeStyle|nil
function M.nick_style(user)
  local provider = _tirc.__nick_style_provider
  if provider then
    return provider(user)
  end
  return nil
end

--- Registers the nick-style provider consulted by `tirc.nick_style`. A single
--- slot: registering replaces any previous provider; pass nil to clear. The
--- slot is reset on `:reload` before the config re-runs.
---@param provider (fun(user: TircUserRef|TircUser): TircThemeStyle|nil)|nil
function M.set_nick_style(provider)
  _tirc.__nick_style_provider = provider
end

--- Joins varargs into one message, stringifying each part (like `print`).
local function format_log(...)
  local parts = {}
  for i = 1, select('#', ...) do
    parts[i] = tostring((select(i, ...)))
  end
  return table.concat(parts, ' ')
end

--- Logging from Lua into the `:debug` pane. Each level accepts any number of
--- arguments, stringified and space-joined like `print`.
---@class TircLog
---@field error fun(...)
---@field warn fun(...)
---@field info fun(...)
---@field debug fun(...)
---@field trace fun(...)
M.log = {}
for _, level in ipairs { 'error', 'warn', 'info', 'debug', 'trace' } do
  M.log[level] = function(...)
    _tirc.__log(level, format_log(...))
  end
end

setmetatable(M, {
  __index = function(_, key)
    if key == 'ui' then
      return _tirc.__get_ui()
    end

    -- `__`-prefixed keys are private host internals; they are only reachable
    -- via `require('_tirc')`, never through the public `tirc` facade.
    if type(key) == 'string' and key:sub(1, 2) == '__' then
      return nil
    end

    return _tirc[key]
  end,
  __newindex = function(t, key, value)
    if key == 'ui' then
      return _tirc.__set_ui(value)
    end

    rawset(t, key, value)
  end,
})

return M
