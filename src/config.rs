//! Configuration for comicbot.
//!
//! The config file is TOML and is created with sane defaults on first run.
//! Relative paths inside the file are resolved against the directory that
//! contains the config file, so a config directory can be moved around.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// When a comic should be rendered automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RenderMode {
    /// Render after every chat message (throttled by `comic.min_interval_secs`).
    Each,
    /// Only render when a user asks for it with `!comic`.
    Manual,
    /// Never render automatically.
    Off,
}

impl Default for RenderMode {
    fn default() -> Self {
        RenderMode::Each
    }
}

/// Comic strip appearance and layout.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ComicConfig {
    /// Width of the generated image in pixels.
    pub width: u32,
    /// Number of panels per row.
    pub columns: usize,
    /// Number of panel rows. The page shows at most `columns * rows` cells.
    pub rows: usize,
    /// Width of the character roster on the right side (0 disables it).
    pub sidebar_width: u32,
    /// Show the roster with all characters of the conversation.
    pub show_sidebar: bool,
    /// Square size of the avatar image inside each panel.
    pub avatar_size: u32,
    /// Font size used for message text.
    pub font_size: f32,
    /// Font size used for the speaker name.
    pub name_font_size: f32,
    /// How long a cell collects messages, in seconds.
    pub cell_window_secs: f32,
    /// Minimum number of seconds between two automatic renders.
    pub min_interval_secs: u64,
    /// Draw the comic on a transparent background (for overlays).
    pub transparent_background: bool,
    /// Show the channel header on top of the strip.
    pub show_header: bool,
    /// Use the built-in comic style (heavier outlines, paper background).
    pub comic_style: bool,
}

impl Default for ComicConfig {
    fn default() -> Self {
        Self {
            width: 1200,
            columns: 3,
            rows: 2,
            sidebar_width: 210,
            show_sidebar: true,
            avatar_size: 96,
            font_size: 17.0,
            name_font_size: 16.0,
            cell_window_secs: 1.0,
            min_interval_secs: 1,
            transparent_background: false,
            show_header: true,
            comic_style: true,
        }
    }
}

impl ComicConfig {
    /// Maximum number of cells that fit on one page.
    pub fn max_cells(&self) -> usize {
        self.columns.max(1) * self.rows.max(1)
    }

    /// Length of the cell window in milliseconds.
    pub fn cell_window_ms(&self) -> i64 {
        (self.cell_window_secs.max(0.05) * 1000.0).round() as i64
    }
}

/// Local web server settings (OAuth callback + status endpoint).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpConfig {
    pub host: String,
    pub port: u16,
    /// Base URL used when posting links into chat. Defaults to
    /// `http://<host>:<port>`.
    pub public_base_url: Option<String>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 3000,
            public_base_url: None,
        }
    }
}

impl HttpConfig {
    pub fn base_url(&self) -> String {
        match &self.public_base_url {
            Some(url) => url.trim_end_matches('/').to_string(),
            None => format!("http://{}:{}", self.host, self.port),
        }
    }
}

/// Twitch IRC connection settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IrcConfig {
    pub server: String,
    pub port: u16,
    /// Minimum delay between two outgoing chat messages, in milliseconds.
    pub send_delay_ms: u64,
    /// Ask Twitch for verbose membership events (join/leave).
    pub request_membership: bool,
}

impl Default for IrcConfig {
    fn default() -> Self {
        Self {
            server: "irc.chat.twitch.tv".to_string(),
            port: 6697,
            send_delay_ms: 1200,
            request_membership: false,
        }
    }
}

/// Font files used by the renderer. Empty values trigger auto-discovery.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FontConfig {
    /// Regular weight font.
    pub regular: Option<PathBuf>,
    /// Bold weight font.
    pub bold: Option<PathBuf>,
    /// Optional "comic" font used for the header and names.
    pub comic: Option<PathBuf>,
}

/// Top level configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Twitch channel to join (without the leading `#`).
    pub channel: String,
    /// Twitch login the bot must connect as. Twitch requires this to match the
    /// account that owns the OAuth token; when empty the token's login is used.
    #[serde(default)]
    pub bot_name: Option<String>,
    /// Twitch application client id.
    pub client_id: String,
    /// Twitch application client secret.
    pub client_secret: String,
    /// OAuth redirect URI, must match the one registered in the app.
    pub redirect_uri: String,

    /// Where tokens and generated comics are stored.
    pub data_dir: PathBuf,
    /// Directory scanned for avatar PNGs that can be assigned to people.
    pub avatars_dir: PathBuf,
    /// Directory scanned for background PNGs.
    #[serde(default = "default_backgrounds_dir")]
    pub backgrounds_dir: PathBuf,
    /// Background drawn behind the contents of every cell.
    #[serde(default)]
    pub background: Option<PathBuf>,
    /// Avatar used when a chatter has no explicit assignment.
    pub default_avatar: Option<PathBuf>,
    /// Avatar names that random assignment must leave alone (for moderators).
    #[serde(default)]
    pub mod_avatars: Vec<String>,
    /// Avatar assignments, keyed by the stable user id of the chatter.
    ///
    /// Older files use the login as the key, they are still read.
    pub avatars: BTreeMap<String, PathBuf>,

    pub render_mode: RenderMode,

    pub comic: ComicConfig,
    pub http: HttpConfig,
    pub irc: IrcConfig,
    pub fonts: FontConfig,

    /// Directory the config was loaded from; relative paths resolve against it.
    #[serde(skip)]
    pub base_dir: PathBuf,
    /// Full path of the config file, used when saving changes back.
    #[serde(skip)]
    pub path: PathBuf,
}

/// Default directory for background pictures.
fn default_backgrounds_dir() -> PathBuf {
    PathBuf::from("backgrounds")
}

impl Default for Config {
    fn default() -> Self {
        Self {
            channel: "salmon85".to_string(),
            bot_name: None,
            client_id: "YOUR_CLIENT_ID".to_string(),
            client_secret: "YOUR_CLIENT_SECRET".to_string(),
            redirect_uri: "http://localhost:3000".to_string(),
            data_dir: PathBuf::from("data"),
            avatars_dir: PathBuf::from("avatars"),
            backgrounds_dir: default_backgrounds_dir(),
            background: None,
            default_avatar: None,
            mod_avatars: Vec::new(),
            avatars: BTreeMap::new(),
            render_mode: RenderMode::Each,
            comic: ComicConfig::default(),
            http: HttpConfig::default(),
            irc: IrcConfig::default(),
            fonts: FontConfig::default(),
            base_dir: PathBuf::from("."),
            path: PathBuf::from("comicbot.toml"),
        }
    }
}

impl Config {
    /// Load the config from `path`, creating it with defaults when missing.
    pub fn load_or_create(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            let text = std::fs::read_to_string(path)?;
            let mut cfg: Config = toml::from_str(&text)?;
            cfg.base_dir = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            cfg.path = path.to_path_buf();
            cfg.apply_env();
            cfg.resolve_paths();
            Ok(cfg)
        } else {
            let mut cfg = Config::default();
            cfg.base_dir = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            cfg.path = path.to_path_buf();
            cfg.apply_env();
            cfg.resolve_paths();
            if let Some(parent) = cfg.base_dir.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            cfg.save(path)?;
            Ok(cfg)
        }
    }

    /// Override values from the environment.
    ///
    /// `COMICBOT_TOKEN` is handled separately by the auth module because it
    /// holds a credential rather than a config value.
    fn apply_env(&mut self) {
        if let Ok(v) = std::env::var("COMICBOT_CHANNEL") {
            self.channel = v;
        }
        if let Ok(v) = std::env::var("COMICBOT_BOT_NAME") {
            let name = v.trim().to_string();
            self.bot_name = if name.is_empty() { None } else { Some(name) };
        }
        if let Ok(v) = std::env::var("COMICBOT_CLIENT_ID") {
            self.client_id = v;
        }
        if let Ok(v) = std::env::var("COMICBOT_CLIENT_SECRET") {
            self.client_secret = v;
        }
        if let Ok(v) = std::env::var("COMICBOT_REDIRECT_URI") {
            self.redirect_uri = v;
        }
        if let Ok(v) = std::env::var("COMICBOT_DATA_DIR") {
            self.data_dir = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("COMICBOT_AVATARS_DIR") {
            self.avatars_dir = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("COMICBOT_BACKGROUNDS_DIR") {
            self.backgrounds_dir = PathBuf::from(v);
        }
    }

    /// Make every path in the config absolute.
    fn resolve_paths(&mut self) {
        self.data_dir = self.abs(&self.data_dir);
        self.avatars_dir = self.abs(&self.avatars_dir);
        self.backgrounds_dir = self.abs(&self.backgrounds_dir);
        self.background = self.background.as_ref().map(|p| self.abs(p));
        self.default_avatar = self.default_avatar.as_ref().map(|p| self.abs(p));
        let assignments: Vec<(String, PathBuf)> = self
            .avatars
            .iter()
            .map(|(login, path)| (login.clone(), self.abs(path)))
            .collect();
        self.avatars = assignments.into_iter().collect();
        if let Some(p) = &self.fonts.regular {
            self.fonts.regular = Some(self.abs(p));
        }
        if let Some(p) = &self.fonts.bold {
            self.fonts.bold = Some(self.abs(p));
        }
        if let Some(p) = &self.fonts.comic {
            self.fonts.comic = Some(self.abs(p));
        }
    }

    fn abs(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.base_dir.join(path)
        }
    }

    /// Directory holding generated comic images.
    pub fn comics_dir(&self) -> PathBuf {
        self.data_dir.join("comics")
    }

    /// File that holds the OAuth token.
    pub fn token_path(&self) -> PathBuf {
        self.data_dir.join("token.json")
    }

    /// Write the config back to disk.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let mut copy = self.clone();
        copy.base_dir = PathBuf::new();
        copy.data_dir = rel(&self.data_dir, &self.base_dir);
        copy.avatars_dir = rel(&self.avatars_dir, &self.base_dir);
        copy.backgrounds_dir = rel(&self.backgrounds_dir, &self.base_dir);
        copy.background = self.background.as_ref().map(|p| rel(p, &self.base_dir));
        copy.default_avatar = self.default_avatar.as_ref().map(|p| rel(p, &self.base_dir));
        for value in copy.avatars.values_mut() {
            *value = rel(value, &self.base_dir);
        }
        let text = toml::to_string_pretty(&copy)?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(path, text)?;
        Ok(())
    }

    /// Store an avatar assignment (login is case insensitive).
    pub fn set_avatar(&mut self, login: &str, path: &Path) {
        self.avatars
            .insert(login.to_ascii_lowercase(), path.to_path_buf());
    }

    /// Remove an avatar assignment, returning the previous value.
    pub fn clear_avatar(&mut self, login: &str) -> Option<PathBuf> {
        self.avatars.remove(&login.to_ascii_lowercase())
    }

    /// Look up the avatar assigned to a login.
    pub fn avatar_for(&self, login: &str) -> Option<&Path> {
        self.avatars
            .get(&login.to_ascii_lowercase())
            .map(|p| p.as_path())
            .or(self.default_avatar.as_deref())
    }
}

fn rel(path: &Path, base: &Path) -> PathBuf {
    path.strip_prefix(base).unwrap_or(path).to_path_buf()
}
