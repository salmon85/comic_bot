//! A small Twitch IRC client.
//!
//! Only the subset that is needed for a chat bot is implemented: TLS
//! connection, IRCv3 tags, `PRIVMSG` parsing, `PING`/`PONG` keep alive and
//! outgoing chat messages with a small rate limiter.

use crate::config::IrcConfig;
use crate::render::Color;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::TlsConnector;

/// A chat message as seen by the bot.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub id: String,
    pub channel: String,
    /// Stable id of the user, it does not change when the name changes.
    pub user_id: String,
    /// Lowercase login of the sender.
    pub login: String,
    pub display_name: String,
    pub text: String,
    pub color: Color,
    pub badges: Vec<String>,
    pub is_mod: bool,
    pub is_sub: bool,
    pub timestamp_ms: i64,
    pub reply_to: Option<String>,
}

/// Events produced by the IRC connection.
#[derive(Debug, Clone)]
pub enum IrcEvent {
    Chat(ChatMessage),
    Notice { text: String },
    System(String),
    Disconnected,
    /// Twitch rejected the login token; wait for a refreshed one.
    AuthFailed,
}

/// Handle used to send chat messages.
#[derive(Clone)]
pub struct IrcClient {
    /// Writer of the current IRC session, if connected.
    outbox: Arc<Mutex<Option<mpsc::UnboundedSender<String>>>>,
    channel: String,
    /// Lets the connection task pick up a refreshed OAuth token.
    token_updates: mpsc::UnboundedSender<String>,
}

impl IrcClient {
    /// Create a client that only collects the outgoing lines (for tests).
    #[cfg(test)]
    pub fn for_test(channel: &str) -> (IrcClient, mpsc::UnboundedReceiver<String>) {
        let (outgoing, rx) = mpsc::unbounded_channel();
        let (token_updates, _) = mpsc::unbounded_channel();
        (
            IrcClient {
                outbox: Arc::new(Mutex::new(Some(outgoing))),
                channel: format!("#{channel}"),
                token_updates,
            },
            rx,
        )
    }

    /// Send a message to the joined channel.
    pub fn send(&self, text: &str) {
        let text = text.replace('\r', " ").replace('\n', " ");
        let Ok(outbox) = self.outbox.lock() else {
            return;
        };
        let Some(outgoing) = outbox.as_ref() else {
            return;
        };
        for line in split_message(&text, 480) {
            let _ = outgoing.send(format!("PRIVMSG {} :{}", self.channel, line));
        }
    }

    /// Hand a refreshed OAuth token to the connection task.
    pub fn update_token(&self, token: String) {
        let _ = self.token_updates.send(token);
    }
}

/// Spawn the IRC connection.
///
/// The returned receiver yields events until the task is dropped. The client
/// reconnects automatically when the connection drops. On an authentication
/// failure it stops reconnecting with the stale token and waits for the caller
/// to call [`IrcClient::update_token`] with a fresh one.
pub async fn connect(
    config: IrcConfig,
    channel: String,
    nickname: String,
    token: String,
    events: mpsc::UnboundedSender<IrcEvent>,
) -> IrcClient {
    let outbox = Arc::new(Mutex::new(None));
    let (token_updates, mut token_rx) = mpsc::unbounded_channel::<String>();
    let client = IrcClient {
        outbox: outbox.clone(),
        channel: format!("#{channel}"),
        token_updates,
    };

    tokio::spawn(async move {
        let mut token = token;
        let mut backoff = Duration::from_secs(2);
        let mut needs_token = false;

        loop {
            if needs_token {
                match token_rx.recv().await {
                    Some(new_token) => {
                        token = new_token;
                        needs_token = false;
                        backoff = Duration::from_secs(2);
                    }
                    None => break,
                }
            }

            let mut auth_failed = false;
            let result = run_session(
                &config,
                &channel,
                &nickname,
                &token,
                &events,
                &outbox,
                &mut auth_failed,
            )
            .await;

            if auth_failed {
                // Retrying a rejected token only repeats the failure. Wait
                // for main to refresh or renew it.
                let _ = events.send(IrcEvent::AuthFailed);
                needs_token = true;
                continue;
            }

            match result {
                Ok(()) => {
                    backoff = Duration::from_secs(2);
                }
                Err(err) => {
                    let _ = events.send(IrcEvent::System(format!("irc: {err}")));
                    let _ = events.send(IrcEvent::Disconnected);
                }
            }

            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    });

    client
}

async fn run_session(
    config: &IrcConfig,
    channel: &str,
    nickname: &str,
    token: &str,
    events: &mpsc::UnboundedSender<IrcEvent>,
    outbox: &Arc<Mutex<Option<mpsc::UnboundedSender<String>>>>,
    auth_failed: &mut bool,
) -> anyhow::Result<()> {
    let connector = tls_connector()?;
    let tcp = tokio::time::timeout(
        Duration::from_secs(15),
        TcpStream::connect((config.server.as_str(), config.port)),
    )
    .await
    .map_err(|_| anyhow::anyhow!("connection attempt timed out"))??;
    tcp.set_nodelay(true).ok();
    let server_name = rustls_pki_types::ServerName::try_from(config.server.clone())
        .map_err(|_| anyhow::anyhow!("invalid server name {}", config.server))?;
    let stream = connector.connect(server_name, tcp).await?;

    let (read_half, stream_write) = tokio::io::split(stream);
    let (write_tx, mut writer_rx) = mpsc::unbounded_channel::<String>();

    // Writer task: chat messages are rate limited, protocol replies are not.
    let delay = Duration::from_millis(config.send_delay_ms);
    let (writer_failed_tx, writer_failed_rx) = tokio::sync::oneshot::channel::<()>();
    let writer = tokio::spawn(async move {
        let mut stream_write = stream_write;
        let mut last = tokio::time::Instant::now() - Duration::from_secs(10);
        while let Some(line) = writer_rx.recv().await {
            let is_chat = line.starts_with("PRIVMSG");
            if is_chat {
                let elapsed = last.elapsed();
                if elapsed < delay {
                    tokio::time::sleep(delay - elapsed).await;
                }
                last = tokio::time::Instant::now();
            }
            if stream_write.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if stream_write.write_all(b"\r\n").await.is_err() {
                break;
            }
            let _ = stream_write.flush().await;
        }
        let _ = writer_failed_tx.send(());
    });

    // Publish the writer so IrcClient::send reaches this session. The slot is
    // cleared again when the session ends, so a later session gets its own.
    if let Ok(mut slot) = outbox.lock() {
        *slot = Some(write_tx.clone());
    }

    let result: anyhow::Result<()> = async {
        let mut reader = BufReader::new(read_half);
        let mut writer_failed_rx = writer_failed_rx;
        let mut line = String::new();

        // Register capabilities, authenticate and join.
        let mut caps = String::from("twitch.tv/tags twitch.tv/commands");
        if config.request_membership {
            caps.push_str(" twitch.tv/membership");
        }
        for command in [
            format!("CAP REQ :{caps}"),
            format!("PASS oauth:{token}"),
            format!("NICK {nickname}"),
            format!("JOIN #{channel}"),
        ] {
            let _ = write_tx.send(command);
        }

        loop {
            line.clear();
            let read = tokio::select! {
                result = tokio::time::timeout(
                    Duration::from_secs(420),
                    reader.read_line(&mut line),
                ) => result,
                _ = &mut writer_failed_rx => {
                    return Err(anyhow::anyhow!("connection lost while writing"));
                }
            };
            let read = match read {
                Ok(Ok(n)) => n,
                Ok(Err(err)) => return Err(err.into()),
                Err(_) => return Err(anyhow::anyhow!("connection timed out")),
            };
            if read == 0 {
                return Err(anyhow::anyhow!("connection closed"));
            }
            let raw = line.trim_end_matches(['\r', '\n']);
            if raw.is_empty() {
                continue;
            }
            if let Some(parsed) = parse_line(raw) {
                match parsed.command.as_str() {
                    "PING" => {
                        let reply = parsed.trailing.unwrap_or_default();
                        let _ = write_tx.send(format!("PONG :{reply}"));
                    }
                    "RECONNECT" => {
                        let _ = events
                            .send(IrcEvent::System("irc: server asked to reconnect".into()));
                        return Err(anyhow::anyhow!("server requested reconnect"));
                    }
                    "PRIVMSG" => {
                        if let Some(message) = chat_message(&parsed, channel) {
                            let _ = events.send(IrcEvent::Chat(message));
                        }
                    }
                    "NOTICE" => {
                        let text = parsed.trailing.clone().unwrap_or_default();
                        if is_login_auth_failure(&text) {
                            *auth_failed = true;
                            return Err(anyhow::anyhow!("login authentication failed"));
                        }
                        let _ = events.send(IrcEvent::Notice { text });
                    }
                    _ => {}
                }
            }
        }
    }
    .await;

    if let Ok(mut slot) = outbox.lock() {
        *slot = None;
    }
    writer.abort();
    result
}

/// Twitch reports a rejected token in a NOTICE.
fn is_login_auth_failure(text: &str) -> bool {
    text.to_ascii_lowercase()
        .contains("login authentication failed")
}

fn tls_connector() -> anyhow::Result<TlsConnector> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        roots.add(cert)?;
    }
    if roots.is_empty() {
        anyhow::bail!("no trusted root certificates found");
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

/// A parsed IRC line.
#[derive(Debug, Clone, Default)]
pub struct IrcMessage {
    pub tags: HashMap<String, String>,
    pub prefix: String,
    pub command: String,
    pub params: Vec<String>,
    pub trailing: Option<String>,
}

/// Parse an IRCv3 line into its parts.
pub fn parse_line(line: &str) -> Option<IrcMessage> {
    let mut rest = line;
    let mut tags = HashMap::new();

    if let Some(stripped) = rest.strip_prefix('@') {
        let end = stripped.find(' ')?;
        let tag_string = &stripped[..end];
        for pair in tag_string.split(';') {
            if pair.is_empty() {
                continue;
            }
            let (key, value) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                None => (pair, ""),
            };
            tags.insert(key.to_string(), unescape_tag(value));
        }
        rest = &stripped[end + 1..];
    }

    let mut prefix = String::new();
    if let Some(stripped) = rest.strip_prefix(':') {
        let end = stripped.find(' ').unwrap_or(stripped.len());
        prefix = stripped[..end].to_string();
        rest = stripped.get(end..).unwrap_or("").trim_start();
    }

    let mut parts = rest.splitn(2, ' ');
    let command = parts.next().unwrap_or_default().to_string();
    let mut params = Vec::new();
    let mut trailing = None;

    if let Some(tail) = parts.next() {
        // Parameters come first, the trailing parameter (everything after the
        // first colon) is always the last one.
        let mut tail = tail.trim_start();
        loop {
            if tail.is_empty() {
                break;
            }
            if let Some(stripped) = tail.strip_prefix(':') {
                trailing = Some(stripped.to_string());
                break;
            }
            match tail.find(' ') {
                Some(index) => {
                    params.push(tail[..index].to_string());
                    tail = tail[index + 1..].trim_start();
                }
                None => {
                    params.push(tail.to_string());
                    break;
                }
            }
        }
    }

    Some(IrcMessage {
        tags,
        prefix,
        command,
        params,
        trailing,
    })
}

fn unescape_tag(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some(':') => out.push(';'),
            Some('s') => out.push(' '),
            Some('\\') => out.push('\\'),
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Build a chat message from a parsed `PRIVMSG`.
fn chat_message(parsed: &IrcMessage, fallback_channel: &str) -> Option<ChatMessage> {
    let target = parsed.params.first().cloned().unwrap_or_default();
    let raw_text = parsed.trailing.clone().unwrap_or_default();
    if raw_text.is_empty() {
        return None;
    }
    // "/me" messages arrive as \u{1}ACTION text\u{1}.
    let text = match raw_text.strip_prefix('\u{1}') {
        Some(rest) => rest
            .strip_prefix("ACTION ")
            .and_then(|body| body.strip_suffix('\u{1}'))
            .map(|body| body.to_string())
            .unwrap_or(raw_text),
        None => raw_text,
    };
    if text.is_empty() {
        return None;
    }
    let channel = target.trim_start_matches('#').to_string();
    if !channel.eq_ignore_ascii_case(fallback_channel) {
        return None;
    }

    let login = parsed
        .prefix
        .split('!')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let display_name = parsed
        .tags
        .get("display-name")
        .cloned()
        .unwrap_or_else(|| login.clone());
    let color = parsed
        .tags
        .get("color")
        .and_then(|value| Color::parse_hex(value))
        .unwrap_or(Color::rgb(102, 102, 102));
    let badges = parsed
        .tags
        .get("badges")
        .map(|value| {
            value
                .split(',')
                .filter_map(|badge| badge.split('/').next().map(|s| s.to_string()))
                .collect::<Vec<String>>()
        })
        .unwrap_or_default();

    Some(ChatMessage {
        id: parsed.tags.get("id").cloned().unwrap_or_default(),
        channel,
        user_id: parsed.tags.get("user-id").cloned().unwrap_or_default(),
        login,
        display_name,
        text,
        color,
        badges,
        is_mod: parsed.tags.get("mod").map(|v| v == "1").unwrap_or(false),
        is_sub: parsed
            .tags
            .get("subscriber")
            .map(|v| v == "1")
            .unwrap_or(false),
        timestamp_ms: parsed
            .tags
            .get("tmi-sent-ts")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        reply_to: parsed.tags.get("reply-parent-msg-id").cloned(),
    })
}

/// Split a long message into chunks that respect Twitch's length limit.
fn split_message(text: &str, limit: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    for word in text.split(' ') {
        if current.is_empty() {
            current.push_str(word);
        } else if current.chars().count() + 1 + word.chars().count() <= limit {
            current.push(' ');
            current.push_str(word);
        } else {
            parts.push(current.clone());
            current.clear();
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    if parts.is_empty() {
        parts.push(String::new());
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_privmsg_with_tags() {
        let line = "@badge-info=;badges=moderator/1;color=#00FF00;display-name=Salmon85;id=abc;mod=1;subscriber=0;user-id=1 :salmon85!salmon85@salmon85.tmi.twitch.tv PRIVMSG #salmon85 :hello there";
        let parsed = parse_line(line).unwrap();
        assert_eq!(parsed.command, "PRIVMSG");
        assert_eq!(parsed.tags.get("display-name").unwrap(), "Salmon85");
        assert_eq!(parsed.params[0], "#salmon85");
        assert_eq!(parsed.trailing.as_deref(), Some("hello there"));

        let message = chat_message(&parsed, "salmon85").unwrap();
        assert_eq!(message.login, "salmon85");
        assert_eq!(message.display_name, "Salmon85");
        assert_eq!(message.text, "hello there");
        assert!(message.is_mod);
        assert_eq!(message.color, Color::rgb(0, 255, 0));
        assert_eq!(message.badges, vec!["moderator".to_string()]);
    }

    #[test]
    fn parses_ping() {
        let parsed = parse_line("PING :tmi.twitch.tv").unwrap();
        assert_eq!(parsed.command, "PING");
        assert_eq!(parsed.trailing.as_deref(), Some("tmi.twitch.tv"));
    }

    #[test]
    fn unescapes_tag_values() {
        let parsed = parse_line("@system-msg=he\\ssaid\\shello\\s! :a PRIVMSG #c :x").unwrap();
        assert_eq!(parsed.tags.get("system-msg").unwrap(), "he said hello !");
    }

    #[test]
    fn keeps_target_and_trailing_apart() {
        let parsed = parse_line(":nick!user@host PRIVMSG #room :a message with spaces").unwrap();
        assert_eq!(parsed.params, vec!["#room".to_string()]);
        assert_eq!(parsed.trailing.as_deref(), Some("a message with spaces"));
        assert_eq!(parsed.prefix, "nick!user@host");
    }

    #[test]
    fn reads_the_user_id() {
        let line = "@display-name=Someone;user-id=12345;id=abc :someone!s@host PRIVMSG #c :hi";
        let parsed = parse_line(line).unwrap();
        let message = chat_message(&parsed, "c").unwrap();
        assert_eq!(message.user_id, "12345");
    }

    #[test]
    fn strips_action_markers() {
        let line = "@display-name=Someone :someone!s@host PRIVMSG #c :\u{1}ACTION waves\u{1}";
        let parsed = parse_line(line).unwrap();
        let message = chat_message(&parsed, "c").unwrap();
        assert_eq!(message.text, "waves");
    }

    #[test]
    fn detects_authentication_failure_notices() {
        assert!(is_login_auth_failure("Login authentication failed"));
        assert!(is_login_auth_failure("login authentication failed"));
        assert!(!is_login_auth_failure("Login successful"));
    }

    #[test]
    fn test_client_sends_messages() {
        let (client, mut outgoing) = IrcClient::for_test("chan");
        client.send("hello");
        assert_eq!(outgoing.try_recv().unwrap(), "PRIVMSG #chan :hello");
    }

    #[test]
    fn splits_long_messages() {
        let text = "word ".repeat(200);
        let parts = split_message(&text, 100);
        assert!(parts.len() > 1);
        for part in &parts {
            assert!(part.chars().count() <= 100);
        }
    }
}
