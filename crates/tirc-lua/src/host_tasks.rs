//! Native async host tasks callable from Lua: process spawning (`_tirc.__spawn`)
//! and HTTP requests (`_tirc.__fetch`). Both run on the tokio runtime and
//! deliver their outcome through an unbounded channel the host drains on the
//! main loop, so the Lua completion callback always runs on the UI thread.
//! The public promise-based API wraps these in `lua/tirc/process.lua`
//! (`process.spawn`) and `lua/tirc/http.lua` (`http.fetch`).

use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use mlua::{Lua, Table};

/// One completed host task: the Lua completion callback (stored in the
/// registry at submission time) plus its outcome.
pub struct HostTaskMessage {
    pub callback: mlua::RegistryKey,
    pub outcome: HostTaskOutcome,
}

pub enum HostTaskOutcome {
    SpawnDone(SpawnResult),
    HttpDone(HttpResponse),
    /// Task-level failure (I/O error while waiting, transport error, ...);
    /// rejects the promise with the message.
    Failed(String),
}

pub struct SpawnResult {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub struct HttpResponse {
    pub status: u16,
    /// Header pairs with lowercased names; duplicate names comma-joined.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// The host-task result channel, stored as Lua app data by the host so the
/// `__spawn`/`__fetch` closures can reach it at call time. Absent in headless
/// tests, in which case submitting a task raises a Lua error (the Lua wrapper
/// turns that into a rejected promise).
pub struct HostTaskSender(pub tokio::sync::mpsc::UnboundedSender<HostTaskMessage>);

/// Fetches the runtime handle and result sender a task submission needs, or
/// fails with a descriptive error (headless tests, or Lua running outside the
/// tokio runtime).
fn task_context(
    lua: &Lua,
) -> mlua::Result<(
    tokio::runtime::Handle,
    tokio::sync::mpsc::UnboundedSender<HostTaskMessage>,
)> {
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| mlua::Error::runtime("host tasks require the tirc runtime"))?;
    let sender = lua
        .app_data_ref::<HostTaskSender>()
        .ok_or_else(|| mlua::Error::runtime("host task channel not installed"))?
        .0
        .clone();
    Ok((handle, sender))
}

fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

/// Backs `_tirc.__spawn(argv, opts, cb)`: spawns `argv` without any shell
/// (stdin null; stdout/stderr piped when capturing, else null) and completes
/// `cb` through the host-task channel. Synchronous failures (empty argv,
/// binary missing, no runtime) raise; the Lua wrapper rejects the promise.
pub(crate) fn lua_spawn(
    lua: &Lua,
    (argv, opts, callback): (Vec<String>, Option<Table>, mlua::Function),
) -> mlua::Result<()> {
    let Some((program, args)) = argv.split_first() else {
        return Err(mlua::Error::runtime("spawn: argv must not be empty"));
    };
    if program.is_empty() {
        return Err(mlua::Error::runtime("spawn: argv[1] must not be empty"));
    }

    let capture = match &opts {
        Some(opts) => opts.get::<Option<bool>>("capture")?.unwrap_or(true),
        None => true,
    };

    let mut command = tokio::process::Command::new(program);
    command.args(args).stdin(Stdio::null());
    // A child inheriting the terminal would draw over the TUI; null or pipe.
    if capture {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
    } else {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }
    if let Some(opts) = &opts {
        if let Some(cwd) = opts.get::<Option<String>>("cwd")? {
            command.current_dir(cwd);
        }
        if let Some(env) = opts.get::<Option<Table>>("env")? {
            for pair in env.pairs::<String, String>() {
                let (key, value) = pair?;
                command.env(key, value);
            }
        }
    }

    let (handle, sender) = task_context(lua)?;
    let mut child = command
        .spawn()
        .map_err(|err| mlua::Error::runtime(format!("spawn: {program}: {err}")))?;
    let callback = lua.create_registry_value(callback)?;
    let program = program.clone();

    handle.spawn(async move {
        let outcome = if capture {
            match child.wait_with_output().await {
                Ok(output) => {
                    log_exit(&program, &output.status);
                    HostTaskOutcome::SpawnDone(SpawnResult {
                        code: output.status.code(),
                        signal: exit_signal(&output.status),
                        stdout: output.stdout,
                        stderr: output.stderr,
                    })
                }
                Err(err) => HostTaskOutcome::Failed(format!("spawn: {program}: {err}")),
            }
        } else {
            match child.wait().await {
                Ok(status) => {
                    log_exit(&program, &status);
                    HostTaskOutcome::SpawnDone(SpawnResult {
                        code: status.code(),
                        signal: exit_signal(&status),
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    })
                }
                Err(err) => HostTaskOutcome::Failed(format!("spawn: {program}: {err}")),
            }
        };
        let _ = sender.send(HostTaskMessage { callback, outcome });
    });

    Ok(())
}

fn log_exit(program: &str, status: &std::process::ExitStatus) {
    if !status.success() {
        log::debug!(target: "tirc::lua", "spawn: {program} exited with {status}");
    }
}

/// The shared HTTP client behind `tirc.http.fetch`. Built lazily; per-request
/// timeouts come from the fetch options.
fn http_client() -> mlua::Result<&'static reqwest::Client> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client);
    }
    let client = reqwest::Client::builder()
        .user_agent(concat!("tirc/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|err| mlua::Error::runtime(format!("fetch: building client: {err}")))?;
    // A concurrent init losing the race is fine; the winner is used.
    let _ = CLIENT.set(client);
    Ok(CLIENT.get().expect("client initialized above"))
}

/// Backs `_tirc.__fetch(url, opts, cb)`: performs the HTTP request on the
/// runtime and completes `cb` through the host-task channel. HTTP error
/// statuses complete successfully (fetch-style, `ok = false`); only transport
/// errors fail.
pub(crate) fn lua_fetch(
    lua: &Lua,
    (url, opts, callback): (String, Option<Table>, mlua::Function),
) -> mlua::Result<()> {
    let (handle, sender) = task_context(lua)?;
    let client = http_client()?;

    let mut method = reqwest::Method::GET;
    let mut timeout = Duration::from_secs(30);

    if let Some(opts) = &opts {
        if let Some(name) = opts.get::<Option<String>>("method")? {
            method = reqwest::Method::from_bytes(name.to_uppercase().as_bytes())
                .map_err(|_| mlua::Error::runtime(format!("fetch: invalid method: {name}")))?;
        }
        if let Some(seconds) = opts.get::<Option<f64>>("timeout")? {
            timeout = Duration::from_secs_f64(seconds.max(0.0));
        }
    }

    let mut builder = client.request(method, &url).timeout(timeout);
    if let Some(opts) = &opts {
        if let Some(headers) = opts.get::<Option<Table>>("headers")? {
            for pair in headers.pairs::<String, String>() {
                let (name, value) = pair?;
                builder = builder.header(name, value);
            }
        }
        if let Some(body) = opts.get::<Option<mlua::String>>("body")? {
            builder = builder.body(body.as_bytes().to_vec());
        }
    }

    let callback = lua.create_registry_value(callback)?;
    handle.spawn(async move {
        let outcome = match run_fetch(builder).await {
            Ok(response) => HostTaskOutcome::HttpDone(response),
            Err(err) => HostTaskOutcome::Failed(format!("fetch: {url}: {err}")),
        };
        let _ = sender.send(HostTaskMessage { callback, outcome });
    });

    Ok(())
}

async fn run_fetch(builder: reqwest::RequestBuilder) -> Result<HttpResponse, reqwest::Error> {
    let response = builder.send().await?;
    let status = response.status().as_u16();

    // Lowercased names; duplicates comma-joined, matching the fetch API's
    // Headers.get semantics.
    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, value) in response.headers() {
        let name = name.as_str().to_ascii_lowercase();
        let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
        match headers.iter_mut().find(|(n, _)| *n == name) {
            Some((_, existing)) => {
                existing.push_str(", ");
                existing.push_str(&value);
            }
            None => headers.push((name, value)),
        }
    }

    let body = response.bytes().await?.to_vec();
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

/// Invokes the completion callback of a finished host task on the current
/// (UI) thread: `cb(true, result_table)` on success, `cb(false, message)` on
/// failure. Called by the host's main loop for every drained message.
pub fn deliver_host_task(lua: &Lua, message: HostTaskMessage) -> mlua::Result<()> {
    let callback: mlua::Function = lua.registry_value(&message.callback)?;
    lua.remove_registry_value(message.callback)?;

    match message.outcome {
        HostTaskOutcome::SpawnDone(result) => {
            let table = lua.create_table()?;
            table.set("code", result.code)?;
            table.set("signal", result.signal)?;
            table.set("stdout", lua.create_string(&result.stdout)?)?;
            table.set("stderr", lua.create_string(&result.stderr)?)?;
            callback.call((true, table))
        }
        HostTaskOutcome::HttpDone(response) => {
            let headers = lua.create_table()?;
            for (name, value) in &response.headers {
                headers.set(name.as_str(), value.as_str())?;
            }
            let table = lua.create_table()?;
            table.set("status", response.status)?;
            table.set("ok", (200..300).contains(&response.status))?;
            table.set("headers", headers)?;
            table.set("body", lua.create_string(&response.body)?)?;
            callback.call((true, table))
        }
        HostTaskOutcome::Failed(message) => callback.call((false, message)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::register_builtin_modules;

    fn lua_with_channel() -> (Lua, tokio::sync::mpsc::UnboundedReceiver<HostTaskMessage>) {
        let lua = Lua::new();
        register_builtin_modules(&lua).unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        lua.set_app_data(HostTaskSender(tx));
        (lua, rx)
    }

    /// Runs `chunk`, expected to store its outcome in the Lua globals
    /// `resolved`/`rejected` via promise handlers.
    fn run(lua: &Lua, chunk: &str) {
        lua.load(chunk).exec().unwrap();
    }

    #[test]
    fn spawn_outside_runtime_rejects() {
        let lua = Lua::new();
        register_builtin_modules(&lua).unwrap();
        run(
            &lua,
            "require('tirc.process')
                .spawn({ 'true' })
                :catch(function(err) rejected = tostring(err) end)",
        );
        let rejected: String = lua.globals().get("rejected").expect("rejected");
        assert!(rejected.contains("runtime"), "{rejected}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_empty_argv_rejects() {
        let (lua, _rx) = lua_with_channel();
        run(
            &lua,
            "require('tirc.process')
                .spawn({})
                :catch(function(err) rejected = tostring(err) end)",
        );
        let rejected: String = lua.globals().get("rejected").expect("rejected");
        assert!(rejected.contains("argv"), "{rejected}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_missing_binary_rejects() {
        let (lua, _rx) = lua_with_channel();
        run(
            &lua,
            "require('tirc.process')
                .spawn({ '/nonexistent-tirc-test-binary' })
                :catch(function(err) rejected = tostring(err) end)",
        );
        let rejected: String = lua.globals().get("rejected").expect("rejected");
        assert!(
            rejected.contains("nonexistent-tirc-test-binary"),
            "{rejected}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_resolves_with_exit_code_and_output() {
        let (lua, mut rx) = lua_with_channel();
        run(
            &lua,
            "require('tirc.process')
                .spawn({ 'sh', '-c', 'echo out; echo err >&2; exit 3' })
                :next(function(result) resolved = result end)",
        );

        let message = rx.recv().await.expect("completion message");
        deliver_host_task(&lua, message).unwrap();

        let resolved: Table = lua.globals().get("resolved").expect("resolved");
        assert_eq!(resolved.get::<i32>("code").unwrap(), 3);
        assert_eq!(resolved.get::<String>("stdout").unwrap(), "out\n");
        assert_eq!(resolved.get::<String>("stderr").unwrap(), "err\n");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_without_capture_resolves_with_empty_output() {
        let (lua, mut rx) = lua_with_channel();
        run(
            &lua,
            "require('tirc.process')
                .spawn({ 'sh', '-c', 'echo swallowed' }, { capture = false })
                :next(function(result) resolved = result end)",
        );

        let message = rx.recv().await.expect("completion message");
        deliver_host_task(&lua, message).unwrap();

        let resolved: Table = lua.globals().get("resolved").expect("resolved");
        assert_eq!(resolved.get::<i32>("code").unwrap(), 0);
        assert_eq!(resolved.get::<String>("stdout").unwrap(), "");
    }

    /// One-shot HTTP server: accepts a single connection, reads the request
    /// head, answers with a canned response, and returns the bound address.
    async fn one_shot_http_server(response: &'static str) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let mut head = Vec::new();
            loop {
                let n = socket.read(&mut buf).await.unwrap();
                head.extend_from_slice(&buf[..n]);
                if n == 0 || head.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        addr
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fetch_resolves_with_status_headers_and_body() {
        let addr = one_shot_http_server(
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-Test: yes\r\n\r\nhello",
        )
        .await;

        let (lua, mut rx) = lua_with_channel();
        run(
            &lua,
            &format!(
                "require('tirc.http')
                    .fetch('http://{addr}/')
                    :next(function(res) resolved = res end)"
            ),
        );

        let message = rx.recv().await.expect("completion message");
        deliver_host_task(&lua, message).unwrap();

        let resolved: Table = lua.globals().get("resolved").expect("resolved");
        assert_eq!(resolved.get::<u16>("status").unwrap(), 200);
        assert!(resolved.get::<bool>("ok").unwrap());
        assert_eq!(resolved.get::<String>("body").unwrap(), "hello");
        let headers: Table = resolved.get("headers").unwrap();
        assert_eq!(headers.get::<String>("x-test").unwrap(), "yes");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fetch_http_error_status_resolves_not_ok() {
        let addr =
            one_shot_http_server("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;

        let (lua, mut rx) = lua_with_channel();
        run(
            &lua,
            &format!(
                "require('tirc.http')
                    .fetch('http://{addr}/')
                    :next(function(res) resolved = res end)"
            ),
        );

        let message = rx.recv().await.expect("completion message");
        deliver_host_task(&lua, message).unwrap();

        let resolved: Table = lua.globals().get("resolved").expect("resolved");
        assert_eq!(resolved.get::<u16>("status").unwrap(), 404);
        assert!(!resolved.get::<bool>("ok").unwrap());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fetch_transport_error_rejects() {
        // Bind-then-drop to get a port that refuses connections.
        let addr = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap()
        };

        let (lua, mut rx) = lua_with_channel();
        run(
            &lua,
            &format!(
                "require('tirc.http')
                    .fetch('http://{addr}/')
                    :catch(function(err) rejected = tostring(err) end)"
            ),
        );

        let message = rx.recv().await.expect("completion message");
        deliver_host_task(&lua, message).unwrap();

        let rejected: String = lua.globals().get("rejected").expect("rejected");
        assert!(rejected.contains("fetch"), "{rejected}");
    }
}
