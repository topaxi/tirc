# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

`tirc` is a terminal IRC client written in Rust (TUI via `ratatui`/`crossterm`). Its
distinguishing feature is that rendering and message formatting are driven by **Lua**
(via `mlua` with LuaJIT). Themes and the user config are Lua scripts; the Rust side owns
IRC connectivity, state, and input handling, and calls into Lua to format every line.

## Commands

```bash
cargo build                 # build
cargo run                   # run the client (reads/creates ~/.config/tirc/init.lua)
cargo test                  # run all Rust tests
cargo test test_next_buffer # run a single test by name
cargo clippy                # lint
cargo fmt                   # format Rust

stylua lua/                 # format Lua (config in stylua.toml: 2-space, 80 col, single quotes)
```

There is no separate Lua test runner; Lua behavior is exercised through Rust tests that
load the builtin modules and default theme (see the `tests` modules in `src/config/mod.rs`
and `src/ui/state.rs`).

## Architecture

### Runtime / event loop (`src/main.rs`)
A multi-threaded tokio runtime (capped at 2 worker threads) runs `root_task`. Two spawned
tasks feed a single `mpsc` channel of `Event`s:
- `poll_input` - crossterm key events plus a 1s `Tick`.
- `connect_irc` - incoming IRC messages from the `ClientStream`.

The main loop drains the channel: `sync_state` -> `render_ui` -> `handle_event`. The
`mlua::Lua` instance is created in `main` and borrowed throughout; it is **not** `Send`, so
it stays on the main loop and is passed by reference into `InputHandler` and the renderer.

### State (`src/ui/state.rs`)
`State` holds all UI state: `mode` (Normal/Command/Insert, vim-like), `nickname`, and an
`IndexMap<String, ChatBuffer>` of buffers keyed by channel/nick name. The default buffer is
`"(status)"`. `push_message` routes a message to a buffer via `get_target_buffer_name`,
which encodes the rules for where channel messages, DMs, self-echoes, and server notices
land - this routing is unit-tested and is the trickiest logic to get right.

Outgoing messages are tagged with a monotonic `label` (IRCv3 labeled-response). When the
server echoes the message back, `push_message_to_buffer` replaces the optimistic local copy
in place by matching that label rather than appending a duplicate.

### Input handling (`src/ui/input.rs`)
`InputHandler` owns the `irc::Client`, the `Tui`, and a `&Lua`. `handle_event` dispatches by
`(Mode, Event)`. Command mode (`:`) parses slash-style commands by `splitn`-matching the
input as a `Box<[&str]>` slice pattern (`m`/`msg`, `me`, `notice`, `j`/`join`, `q`/`quit`,
`nick`, `whois`, `list`, ...). Incoming IRC messages fire the Lua `"message"` event before
being pushed to state.

### Lua integration (`src/config/mod.rs`, `src/lua/mod.rs`, `src/tui/lua.rs`)
- `register_builtin_modules` registers the native `_tirc` runtime module and `include_str!`s
  the bundled Lua modules under `lua/` (`tirc`, `tirc.config`, `tirc.utils`,
  `tirc.tui.themes.default`) into `package.loaded`. It touches no filesystem, so it is
  reusable from tests.
- `load_config` resolves `init.lua` via XDG, writes a default config on first run, prepends
  the config dir to Lua's `package.path`, then evaluates the config and deserializes it into
  `TircConfig` with `lua.from_value`.
- Event callbacks: Lua registers handlers with `tirc.on(name, fn)`, stored under registry
  keys `tirc-event-<name>`. Rust invokes them with `emit_sync_callback`. Event names are
  enumerated in the `EventName` alias in `lua/tirc/init.lua` (e.g. `message`,
  `format-message-text`, `format-message-nickname`, `format-buffer-title`, `format-user`).
- `to_lua_message` (`src/tui/lua.rs`) converts an `irc::proto::Message` into the Lua table
  shape that themes consume (`nick`, `command`, `params`, ...).

### Rendering (`src/tui/`)
`Tui` (`ui.rs`) drives the `ratatui` terminal; `renderer.rs` builds the layout and, for each
message, calls the relevant Lua `format-*` callback to produce styled spans, which are
converted back into `ratatui` `Line`/`Span`s. `wrap.rs` handles unicode-aware line wrapping.
Themes return nested tables of `{ text, style }`; `theme.style{ fg=, bg= }` builds a style on
the Lua side (`create_tirc_theme_lua_module`).

### Lua source layout (`lua/tirc/`)
- `init.lua` - the public `tirc` module (`create_config`, `use`, re-exports `_tirc`).
- `config.lua` - `create_config` shape.
- `tui/theme.lua` - theme helper API.
- `tui/themes/default.lua` - the bundled default theme; the canonical example of how
  `format-*` callbacks are written.

## Conventions

- IRC capabilities requested at connect are listed in `setup_irc`; add new ones there. The
  `EventName` alias and the matching `emit_sync_callback` call sites must stay in sync when
  adding events.
- When changing message-routing or formatting behavior, add/extend the Rust unit tests that
  drive raw IRC lines through `State`/the theme rather than testing manually.
