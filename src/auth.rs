//! Twitch OAuth handling.
//!
//! Two tokens are involved:
//!
//! * an **app token** (`client_credentials`) used for public Helix calls, and
//! * a **user token** (`authorization_code`) with the `chat:read` scope, which
//!   is what Twitch IRC requires.
//!
//! The user token is obtained through the authorization code flow. The local
//! web server that is already part of comicbot acts as the redirect target.

use crate::config::Config;
use crate::https::HttpsClient;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

const TOKEN_URL: &str = "https://id.twitch.tv/oauth2/token";
const AUTHORIZE_URL: &str = "https://id.twitch.tv/oauth2/authorize";
const VALIDATE_URL: &str = "https://id.twitch.tv/oauth2/validate";
const USERS_URL: &str = "https://api.twitch.tv/helix/users";

/// Scopes requested for the bot account.
pub const BOT_SCOPES: &str = "chat:read chat:edit";

/// Extra scopes for the commands that need moderator permissions.
pub const MODERATOR_SCOPES: &str = "chat:read chat:edit moderator:read:chatters \
moderator:read:chat_messages moderator:manage:chat_messages";

/// Token material as returned by Twitch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub expires_in: i64,
    #[serde(default)]
    pub scope: Vec<String>,
    #[serde(default)]
    pub token_type: String,
    /// Unix timestamp (seconds) when the token was obtained.
    #[serde(default)]
    pub obtained_at: i64,
}

impl TokenSet {
    pub fn is_expired(&self) -> bool {
        if self.expires_in <= 0 {
            return false;
        }
        let now = now_secs();
        now >= self.obtained_at + self.expires_in - 60
    }
}

/// Identity of the authenticated account.
#[derive(Debug, Clone)]
pub struct Identity {
    pub client_id: String,
    pub login: String,
    pub user_id: String,
    pub scopes: Vec<String>,
}

/// Public profile of a Twitch account.
#[derive(Debug, Clone)]
pub struct UserProfile {
    pub login: String,
    pub display_name: String,
    pub profile_image: Option<String>,
}

/// Build the URL the user has to open to authorize the application.
pub fn authorize_url(config: &Config, state: &str, scopes: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}&state={}&force_verify=true",
        crate::https::urlencode(&config.client_id),
        crate::https::urlencode(&config.redirect_uri),
        crate::https::urlencode(scopes),
        crate::https::urlencode(state)
    )
}

/// Exchange an authorization code for a user token.
pub async fn exchange_code(
    client: &HttpsClient,
    config: &Config,
    code: &str,
) -> Result<TokenSet> {
    let form = vec![
        ("client_id", config.client_id.clone()),
        ("client_secret", config.client_secret.clone()),
        ("code", code.to_string()),
        ("grant_type", "authorization_code".to_string()),
        ("redirect_uri", config.redirect_uri.clone()),
    ];
    let response = client.post_form(TOKEN_URL, &form, &[]).await?;
    token_from_response(&response)
}

/// Exchange a refresh token for a fresh access token.
pub async fn refresh(
    client: &HttpsClient,
    config: &Config,
    refresh_token: &str,
) -> Result<TokenSet> {
    let form = vec![
        ("client_id", config.client_id.clone()),
        ("client_secret", config.client_secret.clone()),
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.to_string()),
    ];
    let response = client.post_form(TOKEN_URL, &form, &[]).await?;
    token_from_response(&response)
}

/// Request an app access token via the client credentials flow.
pub async fn app_token(client: &HttpsClient, config: &Config) -> Result<String> {
    let form = vec![
        ("client_id", config.client_id.clone()),
        ("client_secret", config.client_secret.clone()),
        ("grant_type", "client_credentials".to_string()),
    ];
    let response = client.post_form(TOKEN_URL, &form, &[]).await?;
    let token = token_from_response(&response)?;
    Ok(token.access_token)
}

fn token_from_response(response: &crate::https::HttpResponse) -> Result<TokenSet> {
    if response.status != 200 {
        bail!(
            "token request failed with status {}: {}",
            response.status,
            response.text()
        );
    }
    let json = response.json()?;
    let access_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("token response without access_token"))?
        .to_string();
    let refresh_token = json
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let expires_in = json.get("expires_in").and_then(|v| v.as_i64()).unwrap_or(0);
    let token_type = json
        .get("token_type")
        .and_then(|v| v.as_str())
        .unwrap_or("bearer")
        .to_string();
    let scope = json
        .get("scope")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    Ok(TokenSet {
        access_token,
        refresh_token,
        expires_in,
        scope,
        token_type,
        obtained_at: now_secs(),
    })
}

/// Validate a token and return its identity.
pub async fn validate(client: &HttpsClient, token: &str) -> Result<Identity> {
    let response = client
        .get(VALIDATE_URL, &[("Authorization", format!("Bearer {token}"))])
        .await?;
    if response.status != 200 {
        bail!(
            "token validation failed with status {}: {}",
            response.status,
            response.text()
        );
    }
    let json = response.json()?;
    Ok(Identity {
        client_id: json
            .get("client_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        login: json
            .get("login")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        user_id: json
            .get("user_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        scopes: json
            .get("scopes")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// Fetch the public profile of the authenticated user.
pub async fn fetch_profile(
    client: &HttpsClient,
    config: &Config,
    user_token: &str,
    app_token: &str,
) -> Result<UserProfile> {
    let headers = vec![
        ("Client-Id", config.client_id.clone()),
        ("Authorization", format!("Bearer {app_token}")),
    ];
    let response = client.get(USERS_URL, &headers).await?;
    if response.status != 200 {
        bail!(
            "helix /users failed with status {}: {}",
            response.status,
            response.text()
        );
    }
    let json = response.json()?;
    let user = json
        .get("data")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .ok_or_else(|| anyhow::anyhow!("helix /users returned no user"))?;
    let _ = user_token;
    Ok(UserProfile {
        login: user
            .get("login")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        display_name: user
            .get("display_name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        profile_image: user
            .get("profile_image_url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    })
}

/// The owner of a channel.
#[derive(Debug, Clone)]
pub struct ChannelOwner {
    pub broadcaster_id: String,
    pub broadcaster_login: String,
    pub broadcaster_name: String,
}

/// Look up the owner of a channel.
pub async fn fetch_channel_owner(
    client: &HttpsClient,
    config: &Config,
    app_token: &str,
    channel: &str,
) -> Result<ChannelOwner> {
    let url = format!(
        "https://api.twitch.tv/helix/channels?broadcaster_login={}",
        crate::https::urlencode(channel)
    );
    let headers = vec![
        ("Client-Id", config.client_id.clone()),
        ("Authorization", format!("Bearer {app_token}")),
    ];
    let response = client.get(&url, &headers).await?;
    if response.status != 200 {
        bail!(
            "helix /channels failed with status {}: {}",
            response.status,
            response.text()
        );
    }
    let json = response.json()?;
    let data = json
        .get("data")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .ok_or_else(|| anyhow::anyhow!("channel {channel} does not exist"))?;
    Ok(ChannelOwner {
        broadcaster_id: data
            .get("broadcaster_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        broadcaster_login: data
            .get("broadcaster_login")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        broadcaster_name: data
            .get("broadcaster_name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

/// A chatter of a channel.
#[derive(Debug, Clone)]
pub struct Chatter {
    pub user_id: String,
    pub login: String,
    pub display_name: String,
}

/// List the chatters of a channel.
///
/// Needs a user token with the `moderator:read:chatters` scope.
pub async fn fetch_chatters(
    client: &HttpsClient,
    config: &Config,
    user_token: &str,
    broadcaster_id: &str,
    moderator_id: &str,
) -> Result<Vec<Chatter>> {
    let url = format!(
        "https://api.twitch.tv/helix/chat/chatters?broadcaster_id={}&moderator_id={}&first=1000",
        crate::https::urlencode(broadcaster_id),
        crate::https::urlencode(moderator_id)
    );
    let headers = vec![
        ("Client-Id", config.client_id.clone()),
        ("Authorization", format!("Bearer {user_token}")),
    ];
    let response = client.get(&url, &headers).await?;
    if response.status != 200 {
        bail!(
            "helix /chat/chatters failed with status {}: {}",
            response.status,
            response.text()
        );
    }
    let json = response.json()?;
    Ok(json
        .get("data")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .map(|item| Chatter {
                    user_id: item
                        .get("user_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    login: item
                        .get("user_login")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    display_name: item
                        .get("user_display_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default())
}

/// A message in the chat of a channel.
#[derive(Debug, Clone)]
pub struct ChatRecord {
    pub message_id: String,
    pub user_id: String,
    pub login: String,
    pub text: String,
}

/// List the recent messages of a channel.
///
/// Needs a user token with the `moderator:read:chat_messages` scope.
pub async fn fetch_chat_messages(
    client: &HttpsClient,
    config: &Config,
    user_token: &str,
    broadcaster_id: &str,
    moderator_id: &str,
    limit: usize,
) -> Result<Vec<ChatRecord>> {
    let url = format!(
        "https://api.twitch.tv/helix/chat/messages?broadcaster_id={}&moderator_id={}&first={}",
        crate::https::urlencode(broadcaster_id),
        crate::https::urlencode(moderator_id),
        limit.clamp(1, 100)
    );
    let headers = vec![
        ("Client-Id", config.client_id.clone()),
        ("Authorization", format!("Bearer {user_token}")),
    ];
    let response = client.get(&url, &headers).await?;
    if response.status != 200 {
        bail!(
            "helix /chat/messages failed with status {}: {}",
            response.status,
            response.text()
        );
    }
    let json = response.json()?;
    Ok(json
        .get("data")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .map(|item| ChatRecord {
                    message_id: item
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    user_id: item
                        .get("user_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    login: item
                        .get("user_login")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    text: item
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default())
}

/// Delete a message from the chat.
///
/// Needs a user token with the `moderator:manage:chat_messages` scope.
pub async fn delete_chat_message(
    client: &HttpsClient,
    config: &Config,
    user_token: &str,
    broadcaster_id: &str,
    moderator_id: &str,
    message_id: &str,
) -> Result<()> {
    let url = format!(
        "https://api.twitch.tv/helix/moderation/chat?broadcaster_id={}&moderator_id={}&message_id={}",
        crate::https::urlencode(broadcaster_id),
        crate::https::urlencode(moderator_id),
        crate::https::urlencode(message_id)
    );
    let headers = vec![
        ("Client-Id", config.client_id.clone()),
        ("Authorization", format!("Bearer {user_token}")),
    ];
    let response = client.request("DELETE", &url, &headers, None).await?;
    if response.status != 204 && response.status != 200 {
        bail!(
            "deleting the message failed with status {}: {}",
            response.status,
            response.text()
        );
    }
    Ok(())
}

/// Store a token on disk with restrictive permissions.
pub fn store_token(path: &Path, token: &TokenSet) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(token)?;
    std::fs::write(path, json)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Load a stored token.
pub fn load_token(path: &Path) -> Result<TokenSet> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("no token found at {}", path.display()))?;
    let token: TokenSet = serde_json::from_str(&text)?;
    Ok(token)
}

/// Remove a stored token.
pub fn clear_token(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// A random state value for the OAuth flow.
pub fn random_state() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = file.read_exact(&mut bytes);
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_contains_parameters() {
        let config = Config::default();
        let url = authorize_url(&config, "abc", BOT_SCOPES);
        assert!(url.starts_with("https://id.twitch.tv/oauth2/authorize?"));
        assert!(url.contains("client_id="));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("state=abc"));
    }

    #[test]
    fn expired_token_is_detected() {
        let token = TokenSet {
            access_token: "x".into(),
            refresh_token: String::new(),
            expires_in: 100,
            scope: vec![],
            token_type: "bearer".into(),
            obtained_at: 0,
        };
        assert!(token.is_expired());
    }
}
