//! Mattermost backend.
//!
//! Authentication: either a personal access token (`token`) or a
//! login_id/password pair. Both yield a bearer token used for all subsequent
//! REST calls and the WebSocket auth challenge.
//!
//! Connection lifecycle:
//!  1. Authenticate -> get session token + user id
//!  2. Resolve team name -> team id
//!  3. List joined channels; emit BufferName + BufferTopic for each
//!  4. Autojoin configured channels not yet joined
//!  5. Emit Ready + Synced
//!  6. Open WebSocket, send authentication_challenge
//!  7. select! loop over WS frames, latency pings, and Commands
//!
//! Echo dedup: outgoing posts carry `pending_post_id = "tirc-{txn}"`. The
//! WS `posted` event echoes the same field back; we parse it as a `TxnId` and
//! set `echo_of` so the UI replaces the pending local copy in place.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use reqwest::Client as HttpClient;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::core::{
    BackendEvent, BackendId, BackendMessage, ChatEvent, Command, EventId, MembershipChange,
    MessageBody, MsgKind, Protocol, TargetId, TxnId, UserRef,
};

use super::{BackendInfo, ChatBackend, CommandReceiver, EventSender};

const RECONNECT_BASE_MS: u64 = 1_000;
const RECONNECT_MAX_MS: u64 = 60_000;
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// Connection parameters, built from the user config.
#[derive(Clone, Debug)]
pub struct MattermostBackendConfig {
    /// Base URL, e.g. `http://localhost:8065`.
    pub url: String,
    /// Personal access token; if set, login_id/password are ignored.
    pub token: Option<String>,
    /// Login id (username or email) for password auth.
    pub login_id: Option<String>,
    /// Password for password auth.
    pub password: Option<String>,
    /// Team name or id to connect to.
    pub team: String,
    /// Channel names to autojoin on connect.
    pub autojoin: Vec<String>,
}

pub struct MattermostBackend {
    id: BackendId,
    config: MattermostBackendConfig,
}

impl MattermostBackend {
    pub fn new(id: BackendId, config: MattermostBackendConfig) -> Self {
        MattermostBackend { id, config }
    }
}

#[async_trait::async_trait]
impl ChatBackend for MattermostBackend {
    fn info(&self) -> BackendInfo {
        BackendInfo {
            id: self.id,
            protocol: Protocol::Mattermost,
            name: self.config.url.clone(),
        }
    }

    async fn run(
        self: Box<Self>,
        events: EventSender,
        mut commands: CommandReceiver,
    ) -> anyhow::Result<()> {
        let mut backoff_ms = RECONNECT_BASE_MS;
        let mut quit = false;

        loop {
            let result =
                connect_once(self.id, &self.config, &events, &mut commands, &mut quit).await;

            if quit {
                break;
            }

            match result {
                Ok(()) => {
                    let _ = events.send(BackendMessage {
                        backend: self.id,
                        event: BackendEvent::Disconnected { reason: None },
                    });
                    break;
                }
                Err(err) => {
                    let _ = events.send(BackendMessage {
                        backend: self.id,
                        event: BackendEvent::Disconnected {
                            reason: Some(err.to_string()),
                        },
                    });
                    log::warn!(
                        "Mattermost connection error: {err:#}; reconnecting in {backoff_ms}ms"
                    );
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms * 2).min(RECONNECT_MAX_MS);
                }
            }
        }

        Ok(())
    }
}

// ---- HTTP session ----

struct MmSession {
    http: HttpClient,
    base_url: String,
    token: String,
    user_id: String,
}

impl MmSession {
    async fn get(&self, path: &str) -> anyhow::Result<Value> {
        let url = format!("{}/api/v4/{}", self.base_url, path);
        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        let body: Value = resp.json().await?;
        if !status.is_success() {
            let msg = body["message"]
                .as_str()
                .unwrap_or("request failed")
                .to_string();
            anyhow::bail!("GET {path} failed ({status}): {msg}");
        }
        Ok(body)
    }

    async fn post(&self, path: &str, body: &Value) -> anyhow::Result<Value> {
        let url = format!("{}/api/v4/{}", self.base_url, path);
        let resp = self.http.post(&url).json(body).send().await?;
        let status = resp.status();
        let resp_body: Value = resp.json().await?;
        if !status.is_success() {
            let msg = resp_body["message"]
                .as_str()
                .unwrap_or("request failed")
                .to_string();
            anyhow::bail!("POST {path} failed ({status}): {msg}");
        }
        Ok(resp_body)
    }

    async fn delete_req(&self, path: &str) -> anyhow::Result<()> {
        let url = format!("{}/api/v4/{}", self.base_url, path);
        let resp = self.http.delete(&url).send().await?;
        if !resp.status().is_success() {
            let body: Value = resp.json().await?;
            let msg = body["message"]
                .as_str()
                .unwrap_or("request failed")
                .to_string();
            anyhow::bail!("DELETE {path} failed: {msg}");
        }
        Ok(())
    }
}

async fn authenticate(config: &MattermostBackendConfig) -> anyhow::Result<MmSession> {
    if let Some(token) = &config.token {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}"))?,
        );
        let http = HttpClient::builder().default_headers(headers).build()?;
        let url = format!("{}/api/v4/users/me", config.url);
        let resp: Value = http.get(&url).send().await?.json().await?;
        let user_id = resp["id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("users/me missing id"))?
            .to_string();
        return Ok(MmSession {
            http,
            base_url: config.url.clone(),
            token: token.clone(),
            user_id,
        });
    }

    let login_id = config.login_id.as_deref().ok_or_else(|| {
        anyhow::anyhow!("Mattermost: need `token` or `user_id` + `password`")
    })?;
    let password = config
        .password
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Mattermost: need `password` for password auth"))?;

    let tmp = HttpClient::new();
    let url = format!("{}/api/v4/users/login", config.url);
    let resp = tmp
        .post(&url)
        .json(&json!({ "login_id": login_id, "password": password }))
        .send()
        .await?;

    if !resp.status().is_success() {
        let body: Value = resp.json().await?;
        anyhow::bail!(
            "Mattermost login failed: {}",
            body["message"].as_str().unwrap_or("unknown error")
        );
    }

    let token = resp
        .headers()
        .get("Token")
        .ok_or_else(|| anyhow::anyhow!("login response missing Token header"))?
        .to_str()?
        .to_string();
    let body: Value = resp.json().await?;
    let user_id = body["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("login response missing user id"))?
        .to_string();

    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))?,
    );
    let http = HttpClient::builder().default_headers(headers).build()?;

    Ok(MmSession {
        http,
        base_url: config.url.clone(),
        token,
        user_id,
    })
}

// ---- Helpers ----

async fn resolve_team(session: &MmSession, name_or_id: &str) -> anyhow::Result<String> {
    if let Ok(team) = session.get(&format!("teams/name/{name_or_id}")).await {
        if let Some(id) = team["id"].as_str() {
            return Ok(id.to_string());
        }
    }
    let team = session.get(&format!("teams/{name_or_id}")).await?;
    team["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("team '{name_or_id}' not found"))
        .map(|s| s.to_string())
}

struct MmChannel {
    id: String,
    name: String,
    display_name: String,
    header: String,
}

fn parse_channel(v: &Value) -> Option<MmChannel> {
    Some(MmChannel {
        id: v["id"].as_str()?.to_string(),
        name: v["name"].as_str()?.to_string(),
        display_name: v["display_name"].as_str().unwrap_or("").to_string(),
        header: v["header"].as_str().unwrap_or("").to_string(),
    })
}

async fn get_joined_channels(
    session: &MmSession,
    team_id: &str,
) -> anyhow::Result<Vec<MmChannel>> {
    let list = session
        .get(&format!("users/me/teams/{team_id}/channels"))
        .await?;
    Ok(list
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("expected array from channels endpoint"))?
        .iter()
        .filter_map(parse_channel)
        .collect())
}

async fn join_channel_by_name(
    session: &MmSession,
    team_id: &str,
    name: &str,
) -> anyhow::Result<MmChannel> {
    let channel = session
        .get(&format!("teams/{team_id}/channels/name/{name}"))
        .await?;
    let channel_id = channel["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("channel '{name}' not found"))?
        .to_string();
    session
        .post(
            &format!("channels/{channel_id}/members"),
            &json!({ "user_id": session.user_id }),
        )
        .await?;
    parse_channel(&channel).ok_or_else(|| anyhow::anyhow!("failed to parse channel '{name}'"))
}

fn send_event(id: BackendId, events: &EventSender, event: ChatEvent) {
    let _ = events.send(BackendMessage {
        backend: id,
        event: BackendEvent::Event(event),
    });
}

fn send_backend(id: BackendId, events: &EventSender, event: BackendEvent) {
    let _ = events.send(BackendMessage { backend: id, event });
}

// ---- Connection ----

/// WS stream/sink aliases.
type WsStream = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

async fn connect_once(
    id: BackendId,
    config: &MattermostBackendConfig,
    events: &EventSender,
    commands: &mut CommandReceiver,
    quit: &mut bool,
) -> anyhow::Result<()> {
    let session = authenticate(config).await?;
    let team_id = resolve_team(&session, &config.team).await?;

    let me = session.get("users/me").await?;
    let username = me["username"].as_str().unwrap_or("unknown").to_string();

    send_backend(id, events, BackendEvent::Ready { nickname: username.clone() });

    let joined = get_joined_channels(&session, &team_id).await?;
    let mut channel_names: HashMap<String, String> = HashMap::new();

    for ch in &joined {
        let display = channel_display_name(ch);
        channel_names.insert(ch.id.clone(), display.clone());
        let target = TargetId::from(ch.id.clone());
        send_event(id, events, ChatEvent::BufferName {
            target: target.clone(),
            name: display,
        });
        if !ch.header.is_empty() {
            send_event(id, events, ChatEvent::BufferTopic {
                target,
                topic: ch.header.clone(),
            });
        }
    }

    let joined_names: std::collections::HashSet<String> =
        joined.iter().map(|c| c.name.clone()).collect();

    for ch_name in &config.autojoin {
        if joined_names.contains(ch_name.as_str()) {
            continue;
        }
        match join_channel_by_name(&session, &team_id, ch_name).await {
            Ok(ch) => {
                let display = channel_display_name(&ch);
                channel_names.insert(ch.id.clone(), display.clone());
                send_event(id, events, ChatEvent::BufferName {
                    target: TargetId::from(ch.id),
                    name: display,
                });
            }
            Err(err) => {
                log::warn!("autojoin '{ch_name}' failed: {err}");
            }
        }
    }

    send_backend(id, events, BackendEvent::Synced);

    let ws_url = make_ws_url(&config.url);
    let (ws, _) = tokio_tungstenite::connect_async(&ws_url).await?;
    let (mut sink, mut stream) = ws.split();

    // WebSocket authentication challenge.
    sink.send(WsMessage::Text(
        serde_json::to_string(&json!({
            "seq": 1,
            "action": "authentication_challenge",
            "data": { "token": session.token }
        }))?
        .into(),
    ))
    .await?;

    // Background latency ping via REST.
    let (ping_tx, mut ping_rx) = mpsc::unbounded_channel::<u64>();
    {
        let http = session.http.clone();
        let base_url = session.base_url.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(PING_INTERVAL);
            loop {
                interval.tick().await;
                let start = Instant::now();
                let url = format!("{base_url}/api/v4/system/ping");
                if http.get(&url).send().await.is_ok() {
                    let ms = start.elapsed().as_millis() as u64;
                    if ping_tx.send(ms).is_err() {
                        break;
                    }
                }
            }
        });
    }

    loop {
        tokio::select! {
            frame = stream.next() => {
                match frame {
                    Some(Ok(WsMessage::Text(text))) => {
                        if let Err(err) = handle_ws_frame(id, events, &mut channel_names, &text) {
                            log::warn!("WS frame error: {err}");
                        }
                    }
                    Some(Ok(WsMessage::Ping(data))) => {
                        let _ = sink.send(WsMessage::Pong(data)).await;
                    }
                    Some(Ok(WsMessage::Close(_))) | None => {
                        anyhow::bail!("WebSocket closed");
                    }
                    Some(Err(err)) => {
                        anyhow::bail!("WebSocket error: {err}");
                    }
                    Some(Ok(_)) => {}
                }
            }

            Some(ms) = ping_rx.recv() => {
                send_backend(id, events, BackendEvent::Latency { ms });
            }

            cmd = commands.recv() => {
                match cmd {
                    None => return Ok(()),
                    Some(Command::Quit { .. }) => {
                        *quit = true;
                        return Ok(());
                    }
                    Some(cmd) => {
                        apply_command(
                            id, &session, events, &mut sink,
                            &team_id, &mut channel_names, &username, cmd,
                        ).await?;
                    }
                }
            }
        }
    }
}

fn channel_display_name(ch: &MmChannel) -> String {
    if !ch.display_name.is_empty() {
        ch.display_name.clone()
    } else {
        ch.name.clone()
    }
}

fn make_ws_url(base: &str) -> String {
    let base = base
        .trim_end_matches('/')
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    format!("{base}/api/v4/websocket")
}

// ---- WS frame handling ----

fn handle_ws_frame(
    id: BackendId,
    events: &EventSender,
    channel_names: &mut HashMap<String, String>,
    text: &str,
) -> anyhow::Result<()> {
    let v: Value = serde_json::from_str(text)?;

    let event_type = match v["event"].as_str() {
        Some(t) => t,
        None => return Ok(()), // ack / non-event frame
    };

    match event_type {
        "posted" | "post_edited" => {
            let data = &v["data"];
            let post_str = data["post"].as_str().unwrap_or("{}");
            let post: Value = serde_json::from_str(post_str)?;

            let channel_id = match post["channel_id"].as_str().filter(|s| !s.is_empty()) {
                Some(c) => c.to_string(),
                None => return Ok(()),
            };
            let target = TargetId::from(channel_id.clone());
            let post_id = post["id"].as_str().unwrap_or_default().to_string();
            let message = post["message"].as_str().unwrap_or_default().to_string();

            let sender_name = data["sender_name"]
                .as_str()
                .unwrap_or_default()
                .trim_start_matches('@')
                .to_string();

            if event_type == "post_edited" {
                if !post_id.is_empty() {
                    send_event(id, events, ChatEvent::Edit {
                        target,
                        id: EventId(post_id),
                        body: MessageBody::plain(message),
                    });
                }
                return Ok(());
            }

            // Decode our pending_post_id to correlate with the local echo.
            let pending_post_id = post["pending_post_id"].as_str().unwrap_or_default();
            let echo_of = pending_post_id
                .strip_prefix("tirc-")
                .and_then(|s| s.parse::<u64>().ok())
                .map(TxnId);

            let time = post["create_at"]
                .as_i64()
                .and_then(chrono::DateTime::from_timestamp_millis);

            let event_id = if post_id.is_empty() {
                None
            } else {
                Some(EventId(post_id))
            };

            send_event(id, events, ChatEvent::Message {
                target: target.clone(),
                id: event_id,
                sender: UserRef::new(sender_name),
                body: MessageBody::plain(message),
                kind: MsgKind::Text,
                echo_of,
                time,
            });

            // Surface channel name on first encounter.
            if !channel_names.contains_key(&channel_id) {
                if let Some(display) = data["channel_display_name"].as_str() {
                    channel_names.insert(channel_id.clone(), display.to_string());
                    send_event(id, events, ChatEvent::BufferName {
                        target: TargetId::from(channel_id),
                        name: display.to_string(),
                    });
                }
            }
        }

        "post_deleted" => {
            let post_str = v["data"]["post"].as_str().unwrap_or("{}");
            let post: Value = serde_json::from_str(post_str)?;
            let post_id = post["id"].as_str().unwrap_or_default();
            let channel_id = post["channel_id"].as_str().unwrap_or_default();
            if !post_id.is_empty() && !channel_id.is_empty() {
                send_event(id, events, ChatEvent::Redaction {
                    target: TargetId::from(channel_id),
                    id: EventId(post_id.to_string()),
                    by: None,
                });
            }
        }

        "user_added" => {
            let channel_id = v["broadcast"]["channel_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let username = v["data"]["username"].as_str().unwrap_or_default();
            if !channel_id.is_empty() && !username.is_empty() {
                send_event(id, events, ChatEvent::Membership {
                    target: TargetId::from(channel_id),
                    who: UserRef::new(username),
                    change: MembershipChange::Join { realname: None },
                    time: None,
                });
            }
        }

        "user_removed" => {
            let channel_id = v["broadcast"]["channel_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let user_id = v["broadcast"]["user_id"].as_str().unwrap_or_default();
            if !channel_id.is_empty() && !user_id.is_empty() {
                send_event(id, events, ChatEvent::Membership {
                    target: TargetId::from(channel_id),
                    who: UserRef::new(user_id),
                    change: MembershipChange::Part { reason: None },
                    time: None,
                });
            }
        }

        "channel_updated" => {
            let ch_str = v["data"]["channel"].as_str().unwrap_or("{}");
            let ch: Value = serde_json::from_str(ch_str)?;
            if let (Some(ch_id), Some(display)) =
                (ch["id"].as_str(), ch["display_name"].as_str())
            {
                channel_names.insert(ch_id.to_string(), display.to_string());
                send_event(id, events, ChatEvent::BufferName {
                    target: TargetId::from(ch_id),
                    name: display.to_string(),
                });
            }
        }

        "direct_added" => {
            if let Some(ch_id) = v["broadcast"]["channel_id"].as_str() {
                if !channel_names.contains_key(ch_id) {
                    let name = v["data"]["teammate_username"]
                        .as_str()
                        .unwrap_or(ch_id)
                        .to_string();
                    channel_names.insert(ch_id.to_string(), name.clone());
                    send_event(id, events, ChatEvent::BufferName {
                        target: TargetId::from(ch_id),
                        name,
                    });
                }
            }
        }

        _ => {}
    }

    Ok(())
}

// ---- Command handling ----

type WsSink = futures::stream::SplitSink<WsStream, WsMessage>;

async fn apply_command(
    id: BackendId,
    session: &MmSession,
    events: &EventSender,
    _sink: &mut WsSink,
    team_id: &str,
    channel_names: &mut HashMap<String, String>,
    my_username: &str,
    cmd: Command,
) -> anyhow::Result<()> {
    match cmd {
        Command::SendMessage {
            target,
            body,
            kind,
            txn,
        } => {
            let text = match kind {
                MsgKind::Action => format!("_{body}_"),
                _ => body.clone(),
            };

            // Optimistic local echo: pending=true (echo_of is Some, no event id).
            send_event(id, events, ChatEvent::Message {
                target: target.clone(),
                id: None,
                sender: UserRef::new(my_username),
                body: MessageBody::plain(&body),
                kind,
                echo_of: Some(txn),
                time: None,
            });

            session
                .post(
                    "posts",
                    &json!({
                        "channel_id": target.as_str(),
                        "message": text,
                        "pending_post_id": format!("tirc-{}", txn.0),
                    }),
                )
                .await
                .unwrap_or_else(|err| {
                    log::warn!("SendMessage failed: {err}");
                    Value::Null
                });
        }

        Command::Join { target } => {
            match join_channel_by_name(session, team_id, target.as_str()).await {
                Ok(ch) => {
                    let display = channel_display_name(&ch);
                    channel_names.insert(ch.id.clone(), display.clone());
                    send_event(id, events, ChatEvent::BufferName {
                        target: TargetId::from(ch.id),
                        name: display,
                    });
                }
                Err(err) => {
                    send_event(id, events, ChatEvent::ServerInfo {
                        target: None,
                        from: None,
                        code: None,
                        text: format!("Join failed: {err}"),
                        raw: None,
                    });
                }
            }
        }

        Command::Part { target, .. } => {
            let path = format!("channels/{}/members/{}", target.as_str(), session.user_id);
            session
                .delete_req(&path)
                .await
                .unwrap_or_else(|err| log::warn!("Part failed: {err}"));
        }

        Command::Redact { id: event_id, .. } => {
            session
                .delete_req(&format!("posts/{}", event_id.0))
                .await
                .unwrap_or_else(|err| log::warn!("Redact failed: {err}"));
        }

        // Unsupported commands are silently ignored.
        _ => {}
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::make_ws_url;

    #[test]
    fn ws_url_http() {
        assert_eq!(
            make_ws_url("http://localhost:8065"),
            "ws://localhost:8065/api/v4/websocket"
        );
    }

    #[test]
    fn ws_url_https() {
        assert_eq!(
            make_ws_url("https://mattermost.example.com/"),
            "wss://mattermost.example.com/api/v4/websocket"
        );
    }
}
