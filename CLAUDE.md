# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

`tirc` is a terminal IRC/Matrix/Mattermost client written in Rust (TUI via
`ratatui`/`crossterm`). Its distinguishing feature is that rendering and message
formatting are driven by **Lua** (via `mlua` with LuaJIT). Themes and the user config are
Lua scripts; the Rust side owns connectivity, state, and input handling, and calls into
Lua to format every line.

## Commands

```bash
cargo build                 # build the whole workspace
cargo run                   # run the client (reads/creates ~/.config/tirc/init.lua)
cargo test --workspace      # run all Rust tests
cargo test test_next_buffer # run a single test by name
cargo clippy --workspace --all-targets # lint
cargo fmt                   # format Rust

stylua crates/tirc-lua/lua  # format Lua (config in stylua.toml: 2-space, 80 col, single quotes)
```

There is no separate Lua test runner; Lua behavior is exercised through Rust tests that
load the builtin modules and default theme (see the `tests` modules in
`crates/tirc-config/src/lib.rs` and `crates/tirc-tui/src/renderer.rs`).

## Workspace layout

The repo is a Cargo workspace (virtual root manifest; all shared dependency versions and
features live in `[workspace.dependencies]`). Dependency edges are strictly acyclic:

- `crates/tirc-core` - protocol-agnostic domain types (`ChatEvent`, `Command`,
  `BufferId`, ...), the backend contract (`core::backend`: `ChatBackend`, `BackendInfo`,
  `BackendHandle`, `spawn`), and the tracing/log setup (`core::logging`). Depends on no
  other workspace crate.
- `crates/tirc-lua` - everything Lua-runtime: mlua helpers, the registry surface
  (`runtime`: `tirc.on` event handlers, `call_formatter`, completion sources, backend
  metadata), the `tirc.tui.theme` style module (`theme`), and the embedded builtin
  modules (`builtins`, `include_str!` of `crates/tirc-lua/lua/`). In debug builds the
  builtins hot-reload from disk via `CARGO_MANIFEST_DIR`.
- `crates/tirc-config` - `TircConfig`/`ServerConfig` deserialization, `load_config`,
  reload, and the persisted stores (`aliases`, `buffer_order`, `ui_prefs`). Dev-depends
  on `tirc-ui` for its theme tests.
- `crates/tirc-ui` - UI state (`State`, `ViewState`, `StoredMessage`, vim-like `Mode`),
  the fuzzy completion engine, and the Rust->Lua event/user converters (`ui::lua`).
  Deliberately free of terminal code.
- `crates/tirc-tui` - the `ratatui` layer: `Tui` terminal lifecycle, the renderer,
  link previews, hyperlinks, wrapping, tmux passthrough.
- `crates/tirc-backend-{irc,matrix,mattermost}` - one crate per protocol; each depends
  only on `tirc-core`. `matrix-sdk` (and its `#![recursion_limit = "256"]`) is isolated
  in `tirc-backend-matrix`.
- `crates/tirc` - the binary: `main.rs` (runtime/event loop) and `input.rs`
  (`InputHandler`, the app orchestrator owning the `Tui`, `&Lua`, and backend handles).

## Architecture

### Runtime / event loop (`crates/tirc/src/main.rs`)
A multi-threaded tokio runtime runs `root_task` on a `LocalSet`. Backends run as `Send`
tasks and feed a single `mpsc` channel of `BackendMessage`s; crossterm events plus a 1s
`Tick` arrive via an `EventStream`. The main loop drains the channel:
`sync_state` -> `render_ui` -> `handle_event`. The `mlua::Lua` instance is created in
`main` and borrowed throughout; it is **not** `Send`, so it stays on the main loop and is
passed by reference into `InputHandler` and the renderer.

### State (`crates/tirc-ui/src/state.rs`)
`State` holds all UI state: `mode` (Normal/Command/Insert, vim-like) and an `IndexMap` of
buffers keyed by `BufferId`. The per-backend status buffer is `"(status)"`.
`push_message` routes a message to a buffer - this routing is unit-tested and is the
trickiest logic to get right. Outgoing messages are tagged with a monotonic transaction
id; when the server echoes the message back, the optimistic local copy is replaced in
place rather than appending a duplicate.

### Input handling (`crates/tirc/src/input.rs`)
`InputHandler` owns the `Tui`, the backend handles, and a `&Lua`. `handle_event`
dispatches by `(Mode, Event)`. Command mode (`:`) parses slash-style commands by
`splitn`-matching the input as a `Box<[&str]>` slice pattern (`m`/`msg`, `me`, `notice`,
`j`/`join`, `q`/`quit`, `nick`, `whois`, `list`, ...). The accepted command set must stay
in sync with `COMMAND_NAMES` in `crates/tirc-ui/src/completion.rs`, which feeds command
completion. Incoming events fire the Lua `"event"` callback before being pushed to state.

### Lua integration (`crates/tirc-lua`, `crates/tirc-config`)
- `builtins::register_builtin_modules` registers the native `_tirc` runtime module and
  `include_str!`s the bundled Lua modules under `crates/tirc-lua/lua/` (`tirc`,
  `tirc.config`, `tirc.utils`, `tirc.class`, the themes) into `package.loaded`. In
  release/test builds it touches no filesystem, so it is reusable from tests.
- `load_config` (tirc-config) resolves `init.lua` via XDG, writes a default config on
  first run, prepends the config dir to Lua's `package.path`, then evaluates the config
  and deserializes it into `TircConfig` with `lua.from_value`.
- Event callbacks: Lua registers handlers with `tirc.on(name, fn)`, stored under registry
  keys `tirc-event-<name>`. Rust invokes them with `runtime::emit_event`; valid names are
  the `runtime::EventName` enum. Theme formatters live on the `tirc.ui` object and are
  invoked with `runtime::call_formatter`.
- `ui::lua::to_lua_event` (tirc-ui) converts a stored message into the Lua table shape
  that themes consume.

### Rendering (`crates/tirc-tui/src/`)
`Tui` (`ui.rs`) drives the `ratatui` terminal; `renderer.rs` builds the layout and, for
each message, calls the relevant Lua formatter to produce styled spans, which are
converted back into `ratatui` `Line`/`Span`s. `wrap.rs` handles unicode-aware line
wrapping. Themes return nested tables of `{ text, style }`; `theme.style{ fg=, bg= }`
builds a style on the Lua side (`tirc_lua::theme::create_tirc_theme_lua_module`).

### Lua source layout (`crates/tirc-lua/lua/tirc/`)
- `init.lua` - the public `tirc` module (`create_config`, `use`, re-exports `_tirc`).
- `config.lua` - `create_config` shape.
- `tui/theme.lua` - theme helper API.
- `tui/themes/default.lua` - the bundled default theme; the canonical example of how
  formatter callbacks are written.

## Conventions

- The `runtime::EventName` enum and the matching `emit_event` call sites must stay in
  sync when adding events.
- When changing message-routing or formatting behavior, add/extend the Rust unit tests
  that drive normalized events through `State`/the theme rather than testing manually.
- New shared dependencies go into `[workspace.dependencies]` in the root `Cargo.toml`;
  member crates reference them with `{ workspace = true }`. Keep the mlua feature set
  defined only there (mlua-sys links native LuaJIT, so exactly one configuration must
  exist in the graph).
