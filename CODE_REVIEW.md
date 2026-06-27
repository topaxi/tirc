# Code Review: `tirc`

A critical review of the codebase, written with the knowledge that this was your
first Rust project started ~3 years ago. The goal is not to make you feel bad about
old code - a lot of it is genuinely good - but to point out the things that an
experienced Rust eye snags on, and to link concepts worth revisiting.

Overall verdict: **the architecture is sound and the hard part (Lua-driven rendering,
IRC message routing) is well-factored and well-tested.** The weak spots are concentrated
in three areas: error handling (`unwrap` everywhere), the async model (blocking calls on
the async runtime), and a few latent bugs that the tests don't reach. None of these are
beginner-embarrassing; they're the normal next layer of polish.

---

## 1. The things that are actually broken

These are real bugs, not style nits. Fix these first.

### 1.1 Dead filter from an operator-precedence mistake - `renderer.rs:182`

```rust
.filter(|message| !message.message.width() > 0)
```

This does not do what it looks like. `!` binds tighter than `>`, so Rust parses it as
`(!message.message.width()) > 0`. `!` on a `usize` is the *bitwise complement*, not a
boolean negation. For width `0` you get `!0 == usize::MAX > 0 == true`; for width `5`
you get `!5 == huge > 0 == true`. **The predicate is `true` for every possible value**,
so the filter does nothing.

You meant `message.message.width() > 0` (keep non-empty messages). It happens to be
harmless today only because `render_message` already returns `None` for empty span lists,
so the filter is dead code on top of being wrong. Clippy doesn't catch this because the
expression is type-valid. Fix:

```rust
.filter(|message| message.message.width() > 0)
```

Concept: Rust's unary operators bind tighter than binary ones, and `!` is overloaded for
both boolean *and* bitwise NOT (`std::ops::Not`). When a boolean expression contains `!`
plus a comparison, parenthesize. See the [operator precedence table](https://doc.rust-lang.org/reference/expressions.html#expression-precedence).

### 1.2 Only the *first* event handler ever runs - `config/mod.rs:106-114`

`register_event` happily appends multiple callbacks per event into a table, but
`emit_sync_callback` only ever invokes the first one:

```rust
#[allow(clippy::never_loop)]
for func in tbl.sequence_values::<mlua::Function>() {
    return func?.call(args);   // returns on the first iteration, always
}
```

The `#[allow(clippy::never_loop)]` is a tell - you silenced the lint that was trying to
warn you. For a "prepare for plugin API" goal (per your recent commits) this is a
blocker: two plugins registering a `message` handler means the second is silently ignored.

There's also a semantic question you need to answer: for `format-*` events you want the
*first non-nil* result (a formatter "wins"), but for `message` side-effect events you want
to run *all* handlers. Those are two different combinators. Suggested shape:

```rust
// Side-effect events: run every handler.
pub fn emit(lua: &Lua, name: &str, args: impl IntoLuaMulti + Clone) -> mlua::Result<()> {
    if let Value::Table(tbl) = lua.named_registry_value(&decorated(name))? {
        for func in tbl.sequence_values::<Function>() {
            func?.call::<()>(args.clone())?;
        }
    }
    Ok(())
}

// Formatter events: first handler that returns a non-nil value wins.
pub fn emit_first(lua: &Lua, name: &str, args: impl IntoLuaMulti + Clone) -> mlua::Result<Value> {
    if let Value::Table(tbl) = lua.named_registry_value(&decorated(name))? {
        for func in tbl.sequence_values::<Function>() {
            let v = func?.call::<Value>(args.clone())?;
            if !v.is_nil() { return Ok(v); }
        }
    }
    Ok(Value::Nil)
}
```

### 1.3 Blocking I/O on the async runtime - `main.rs:137-159`

`poll_input` runs inside `rt.spawn(...)`, i.e. on a tokio worker thread, but
`crossterm::event::poll` and `crossterm::event::read` are **synchronous, blocking** calls.
You're parking an async worker thread on a blocking syscall. With `worker_threads(2)` and
two long-lived tasks (`poll_input`, `connect_irc`), you can starve the runtime: if the IRC
task needs the thread that `poll_input` is blocking, things stall. The extra
`tokio::time::sleep(10ms)` is a band-aid that also adds up-to-10ms input latency.

The idiomatic fix is crossterm's async event stream (feature `event-stream`), which gives
you a `futures::Stream` you can `select!` on alongside the IRC stream - no manual mpsc
fan-in, no sleep, no blocking:

```rust
use crossterm::event::EventStream;
let mut events = EventStream::new();
loop {
    tokio::select! {
        Some(Ok(ev)) = events.next() => { /* handle key */ }
        Some(msg)     = irc_stream.next() => { /* handle irc */ }
        _ = tick.tick() => { /* tokio::time::interval for the 1s tick */ }
    }
}
```

Must-read: Alice Ryhl, ["Async: What is blocking?"](https://ryhl.io/blog/async-what-is-blocking/).
It's the single most useful article for understanding why this matters. Also see
[`tokio::select!`](https://tokio.rs/tokio/tutorial/select) and
[`tokio::time::interval`](https://docs.rs/tokio/latest/tokio/time/fn.interval.html) (a
cleaner tick than the manual `Instant` bookkeeping you have now).

### 1.4 No panic hook - a panic leaves the terminal wrecked

`Tui::drop` restores the terminal (good), but if *anything* panics while the alternate
screen + raw mode is active, the unwind may not run `Drop` in a usable order, and the user
is left with a garbled, no-echo terminal. Worse, `Drop` itself uses `.unwrap()`
(`ui.rs:73,80`), so a failure during cleanup *while already panicking* is a double-panic =
instant abort.

The standard ratatui pattern is to install a panic hook that restores the terminal first,
then runs the default hook:

```rust
let original = std::panic::take_hook();
std::panic::set_hook(Box::new(move |info| {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
    original(info);
}));
```

See ratatui's [panic-hook recipe](https://ratatui.rs/recipes/apps/panic-hooks/). In `Drop`
use `let _ = ...` instead of `.unwrap()` so cleanup is infallible.

---

## 2. Error handling - the dominant theme

This is the biggest single area where the code reads as "early Rust." You reach for
`.unwrap()`/`.expect()` in places where a real error is both possible and recoverable.

### 2.1 Panics across the FFI boundary - `main.rs:26,36`

```rust
sender.send_privmsg(target, message).unwrap();
```

These run *inside* Lua callbacks. A network hiccup makes `send_privmsg` return `Err`, you
`unwrap`, and you panic through the mlua C boundary. At best mlua turns it into a Lua
error; at worst it's UB-adjacent. Return the error to Lua instead:

```rust
lua.create_function(move |_, (target, message): (String, String)| {
    sender.send_privmsg(target, message).map_err(mlua::Error::external)
})?
```

`mlua::Error::external` is exactly the bridge for "a Rust error happened in a callback" -
you already use it in `date_time.rs`, so apply it consistently.

### 2.2 `unwrap` in the hot render path - `renderer.rs` and `message.rs:16`

`date_time_to_table(lua, date_time).unwrap()` (`renderer.rs:242`) and
`to_lua_message(lua, &message).unwrap()` (`message.rs:16`) panic the *entire app* if Lua
table creation ever fails (e.g. out of memory, or a future refactor makes these fallible
for real). Rendering should degrade, not crash. You already have the right instinct
elsewhere in the same file (`.unwrap_or_else(|_| vec![])`); apply it here too.

### 2.3 The error is printed where nobody can read it - `main.rs:103`

```rust
eprintln!("Error: {:?}", err);
```

This fires while the terminal is still in raw mode / alternate screen, so the message is
mangled or invisible, *then* `Tui::drop` clears the screen on the way out. Capture the
error, break the loop, restore the terminal, *then* print. Structurally: have `root_task`
return the `Result`, let `Tui` drop, and print in `main`.

### 2.4 `expect` in config loading - `config/mod.rs`, `main.rs:162-172`

`server_config.nickname.first().expect("No nickname found")`, `servers.first().unwrap()`,
etc. These are user-config errors (someone wrote an `init.lua` with an empty `nickname`
list) being handled as programmer errors. They deserve a real `anyhow::bail!` with a
message that tells the user *which field* in *which server* is wrong. A config file is
untrusted input; treat it like one.

### Reading on error handling
- The Book, [ch. 9](https://doc.rust-lang.org/book/ch09-00-error-handling.html) -
  `panic!` vs `Result`, and the "is this a bug or an expected failure?" framing.
- [`anyhow`](https://docs.rs/anyhow) you already use well; pair it with `.context("...")`
  to annotate errors as they bubble up (e.g. `.context("reading init.lua")`).
- For the eventual plugin API, consider [`thiserror`](https://docs.rs/thiserror) for a
  typed library-style error enum at the boundary, keeping `anyhow` for the app glue. The
  rule of thumb: `thiserror` for libraries (callers match on variants), `anyhow` for
  binaries (you just want a backtrace and context).

---

## 3. Architecture & design notes

### 3.1 Reconsider the threading model

You enable mlua's `send` feature and run a 2-worker multi-thread runtime, but the actual
design is single-threaded: `Lua` is `!Send` in practice (it lives on the main loop and is
borrowed by `InputHandler<'lua>` and the renderer), and all state mutation happens on the
main loop. The two spawned tasks are pure I/O producers feeding one channel.

That means you could switch to a **current-thread runtime + `LocalSet`** (or just the
`select!` loop from 1.3), drop the `send` feature on mlua, and lose nothing. Fewer threads,
fewer `Send`/`Arc` requirements, simpler reasoning. The `Arc<Sender>` +
`Arc::clone`-per-function dance in `create_lua_irc_sender` (`main.rs:19-39`) largely exists
to satisfy `Send`; `irc::client::Sender` is already cheap to clone, so without the `send`
feature you may not need the `Arc` at all.

Concept: ["Send and Sync"](https://doc.rust-lang.org/nomicon/send-and-sync.html) and
tokio's [current-thread vs multi-thread runtime](https://docs.rs/tokio/latest/tokio/runtime/index.html#runtime-configurations).
Also note `usize::min(2, num_cpus::get())` can drop the `num_cpus` dependency entirely:
[`std::thread::available_parallelism()`](https://doc.rust-lang.org/std/thread/fn.available_parallelism.html)
has been stable since Rust 1.59.

### 3.2 Event dispatch by string-formatting the registry key

`format!("tirc-event-{}", name)` runs on *every* emit, i.e. potentially several times per
render frame, each allocating a `String`. The registry is also a slightly awkward place to
keep this. A cleaner model for the plugin era: hold the handlers Rust-side in a
`HashMap<&'static str, Vec<RegistryKey>>` (or an enum-keyed array, since `EventName` is a
closed set). Lookups become hash/array indexing, no allocation, and the set of valid event
names is enforced by the type system rather than by a stringly-typed convention that has to
stay in sync with the Lua `---@alias EventName` (which today is maintained by hand - easy to
drift). Consider a Rust `enum EventName` with a `&'static str` mapping as the single source
of truth.

### 3.3 The `flatten_lua_value` styled-span heuristic is fragile - `renderer.rs:75-107`

"A table of length 2 whose second element is a table is a styled span" is a heuristic that
will eventually misfire. `{ {..}, {..} }` (two child tables, not text+style) can be
mis-read as styled if `from_value::<Style>` happens to succeed on the second (it succeeds
on an empty table, producing a default `Style`). It works for the current theme because the
theme author knows the rule, but it's an implicit contract. Two ways to harden it:

- Make the style a *tagged* value: have `theme.style{...}` return a table with a marker
  field (`__tirc_style = true`) or a distinct metatable, and check identity instead of
  shape. This removes all ambiguity.
- Or accept an explicit `{ text = ..., style = ... }` record shape.

The recursion itself is clean and the tests for it are good - it's only the *detection* that
worries me.

### 3.4 `Renderer` is a zero-field struct that never uses `self`

```rust
pub struct Renderer {}
```

Every method takes `&self` but none read state. This is just a namespace. Either make the
methods free functions in the module, or - if you want them grouped - associated functions
(`fn render(f, state, lua, input)` without `&self`). The `Default`/`new` boilerplate
disappears. (If you anticipate caching rendered lines on the renderer later, keeping the
struct is fine - but then actually put a cache in it; see 3.5.)

### 3.5 Re-rendering and re-sorting every frame

- `sync_state` (`input.rs:45`) clones the channel and user lists from the IRC client on
  *every* loop iteration, including on every keystroke and every 1s tick.
- `render_users` sorts the user list on every frame (you flag this yourself with a TODO).
- `render_buffer_title` clones `server`/`nickname`/`current_buffer` Strings each frame.

For an IRC client with small lists this is genuinely fine - don't prematurely optimize. But
the *structural* fix you hint at in the TODO is the right one: keep users in a sorted
structure (e.g. a `BTreeSet` keyed by `(access_priority, nick)`) and update it on
membership events, so render is pure read. Tie this to a "render only when state changed"
flag (a dirty bit set by `handle_event`) and you stop redrawing on idle ticks.

### 3.6 Scroll position is stored but never used

`ChatBuffer::scroll_position` (`state.rs:22`) exists, and `render_messages` has a
`// TODO: Make message list scrollable`. The field is dead until scrolling lands. Either
wire it up or drop it so the struct doesn't imply a capability that isn't there. The
`.take(height + height/2)` heuristic (`renderer.rs:176`) is a reasonable stopgap but will
fight you once scrolling exists - real scrolling wants to render from `scroll_position`, not
always from the tail.

---

## 4. Smaller refactors and idioms

- **`renderer.rs:188`** `initial_indent.len() > 0` -> `!initial_indent.is_empty()` (clippy
  flags this). Same energy as the precedence bug: prefer `is_empty()`.
- **`state.rs:119`** `map_or(false, ...)` -> `is_some_and(...)` (clippy flags this).
- **`ui.rs:62`** `render(&mut self, _irc: &Client, ...)` - the `_irc` parameter is unused.
  Drop it from the signature rather than carrying a `_`-prefixed dead arg through the call
  chain.
- **`main.rs:70`** passing `rt: &Runtime` into the very task that `rt.block_on` is running
  just to call `rt.spawn` is circular. Inside `block_on` you're already in the runtime
  context, so plain `tokio::spawn(...)` works and you can delete the parameter.
- **`config/mod.rs:122-156`** `get_version_lua_value` builds a Lua table with a
  `__tostring` metatable by hand, re-parsing `major/minor/patch` as `u8` inside the
  closure. That's a lot of `.expect()` for a version string. Since you already pull in
  `serde`/`LuaSerdeExt`, consider serializing a small `#[derive(Serialize)]` struct, or at
  minimum store the version once and format it directly.
- **`main.rs:84`** the `label` tag uses a process-global `AtomicUsize` counter
  (`input.rs:18`). Fine, but note IRCv3 labels are per-connection; if you ever support
  reconnect/multi-server, scope the counter to the connection.
- **Magic numbers**: `mpsc::channel(16)` (`main.rs:77`), `Duration::from_millis(1000)`,
  `from_millis(10)`. Name them (`const TICK: Duration = ...`) so intent is visible.
- **`split_params` in `tui/lua.rs`** reimplements IRC parameter parsing by hand even though
  `irc::proto` already parsed the message into a `Command`. You're round-tripping
  `Command -> String -> re-split`. If `irc-proto` exposes the structured params, prefer
  that; hand-rolled IRC tokenizing is a classic source of off-by-one bugs (trailing `:`,
  empty trailing param, etc.). At minimum this deserves its own focused unit tests (the
  current tests exercise it only indirectly).
- **`wrap.rs`** carries `#[allow(dead_code)]` / `#[allow(unused)]` on `wrap_text`,
  `StyledWord::new`, etc. - leftovers from the upstream copy. Trim what you don't use; dead
  code with `allow` attributes hides real dead-code warnings later. Keep the attribution
  comment (good practice that you did this).

---

## 5. Testing

Genuinely a strong point - the buffer-routing tests (`state.rs`), the message-shaping tests
(`tui/lua.rs`), and the span-flattening tests (`renderer.rs`) cover the trickiest logic and
read clearly. Three gaps worth closing:

1. **No test reaches the `renderer.rs:182` bug** because the wrapping/layout path isn't
   unit-tested. A test that runs a buffer with one empty and one non-empty message through
   the filter would have caught it.
2. **`split_params`** has no direct tests despite being fiddly - add cases for trailing
   `:`, no trailing, empty params, and a param that itself contains `:`.
3. **`emit_sync_callback` multi-handler behavior** - a test registering two handlers and
   asserting both run (once you fix 1.2) locks in the contract.

You could also adopt `cargo clippy -- -D warnings` in CI (you only have 2 warnings today)
so the next `len() > 0` or `never_loop` gets caught automatically. Given you have a
`dependabot.yml`, you clearly already value automation here.

---

## 6. What's good (so you know what to keep doing)

- **Message-to-buffer routing** (`state.rs`) is the genuinely hard domain logic, and it's
  cleanly separated, thoroughly commented with the *why*, and well tested. The comments
  explaining echo vs. server-prefix vs. outgoing are exactly the kind that age well.
- **The Lua boundary types** (`to_lua_message`, `to_lua_user`) are documented with the
  resulting Lua shape inline - excellent for anyone writing a theme.
- **Numeric replies surfaced as symbolic names** (`RPL_WELCOME` instead of `001`) is a
  thoughtful touch that makes themes readable.
- **The label-based echo dedup** (`push_message_to_buffer`) is a clever use of
  labeled-response to avoid double-printing your own messages.
- **`register_builtin_modules` being filesystem-free** so it's reusable from tests is a
  deliberate, mature design decision.
- **`wrap.rs`** is a careful adaptation with the upstream license preserved and the
  divergence from the original documented.

---

## Suggested order of attack

1. Fix the three real bugs: precedence (1.1), single-handler dispatch (1.2), and add the
   panic hook (1.4).
2. Sweep `unwrap`/`expect` in the I/O and callback paths into real error handling (§2).
3. Move to the `select!` + `EventStream` loop (1.3) - this also lets you delete the manual
   tick bookkeeping and the 10ms sleep.
4. Then the architectural cleanups (§3) as you build toward the plugin API.

The bones are good. Most of this is the difference between "works" and "robust," which is
exactly the right thing to be sharpening on a project you're revisiting with more
experience.
