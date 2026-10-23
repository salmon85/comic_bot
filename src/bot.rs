//! The bot itself: chat state, commands, comic generation and the local web
//! routes.

use crate::avatars;
use crate::comic::{self, Cell, Character, ComicMessage, ComicPage};
use crate::config::{Config, RenderMode};
use crate::fonts::Fonts;
use crate::irc::{ChatMessage, IrcClient};
use crate::render::Color;
use crate::server::{Handler, HandlerFuture, HttpRequest, HttpResponse};
use std::collections::{hash_map::DefaultHasher, HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

/// What the web server tells the bot about an OAuth callback.
#[derive(Debug, Clone)]
pub enum AuthEvent {
    /// The user came back from the Twitch authorization page.
    Code(String),
    /// The callback was malformed.
    Failed(String),
}

/// Everything the bot remembers about a chatter.
#[derive(Debug, Clone)]
pub struct Participant {
    pub display_name: String,
    pub color: Color,
    pub badges: Vec<String>,
    pub avatar: Option<PathBuf>,
    pub message_count: u32,
    pub is_mod: bool,
    pub is_sub: bool,
}

/// Mutable runtime state.
#[derive(Debug, Default)]
pub struct BotState {
    /// The conversation as a rolling window of cells. A cell holds every
    /// message that arrived within the configured cell window.
    pub cells: VecDeque<Cell>,
    /// The chatters of the conversation, keyed by their stable user id.
    pub participants: HashMap<String, Participant>,
    /// Avatar assignments made during this session, keyed by user id.
    pub avatar_overrides: HashMap<String, PathBuf>,
    /// The name a user is currently known under.
    pub identities: HashMap<String, Identity>,
    /// Login to user id.
    pub by_login: HashMap<String, String>,
    /// Display name to user id, lower case.
    pub by_display_name: HashMap<String, String>,
    /// The chatters of the channel as reported by twitch.
    pub roster: HashMap<String, Identity>,
    /// Ids of messages that were already processed.
    pub seen_ids: HashSet<String>,
    pub last_render: Option<Instant>,
    pub comic_count: u32,
    pub last_comic: Option<String>,
    pub bot_login: String,
    /// Stable user id of the bot account.
    pub bot_user_id: String,
    /// Background drawn behind every cell.
    pub background: Option<PathBuf>,
    /// Messages arrived since the last render.
    pub dirty: bool,
}

/// The name of a user.
#[derive(Debug, Clone)]
pub struct Identity {
    pub user_id: String,
    pub login: String,
    pub display_name: String,
}

impl BotState {
    /// Total number of messages currently held in the cells.
    pub fn message_count(&self) -> usize {
        self.cells.iter().map(|cell| cell.messages.len()).sum()
    }

    /// Add one message to the rolling window of cells.
    fn push_message(&mut self, message: ComicMessage, window_ms: i64, max_cells: usize) {
        let starts_new_cell = self.cells.back().map_or(true, |cell| {
            message.timestamp_ms < cell.first_time_ms
                || message.timestamp_ms.saturating_sub(cell.first_time_ms) >= window_ms
        });
        if starts_new_cell {
            self.cells.push_back(Cell {
                first_time_ms: message.timestamp_ms,
                messages: vec![message],
            });
        } else if let Some(cell) = self.cells.back_mut() {
            cell.messages.push(message);
        }
        while self.cells.len() > max_cells {
            self.cells.pop_front();
        }
    }
}

/// The bot.
pub struct Bot {
    pub config: Config,
    pub fonts: Fonts,
    pub state: Arc<Mutex<BotState>>,
    /// Texts the bot sent itself, used to recognise the echo of its own
    /// messages without dropping the messages of the same account.
    sent: Mutex<VecDeque<(String, Instant)>>,
    /// Optional native window that shows the newest comic.
    display: Option<crate::display::ComicDisplay>,
}

impl Bot {
    pub fn new(config: Config, fonts: Fonts) -> Self {
        let mut state = BotState::default();
        state.background = config.background.clone();
        Self {
            config,
            fonts,
            state: Arc::new(Mutex::new(state)),
            sent: Mutex::new(VecDeque::new()),
            display: None,
        }
    }

    /// Attach the native window that shows every rendered page.
    pub fn attach_display(&mut self, display: crate::display::ComicDisplay) {
        self.display = Some(display);
    }

    /// Send an answer to the chat and remember it.
    ///
    /// Every answer of the bot is prefixed with `BOT:`, the bot only answers
    /// the commands of the users. Twitch sends the messages of an account back
    /// to that account, so the bot has to recognise its own messages to avoid
    /// answering itself.
    pub fn send(&self, client: &IrcClient, text: &str) {
        let text = format!("BOT: {text}");
        {
            let mut sent = self.sent.lock().unwrap();
            sent.push_back((text.clone(), Instant::now()));
            while sent.len() > 50 {
                sent.pop_front();
            }
        }
        eprintln!("sent: {text}");
        client.send(&text);
    }

    /// Was this text sent by the bot itself a moment ago?
    fn sent_by_bot(&self, text: &str) -> bool {
        let mut sent = self.sent.lock().unwrap();
        let now = Instant::now();
        while let Some((_, when)) = sent.front() {
            if now.duration_since(*when) > Duration::from_secs(60) {
                sent.pop_front();
            } else {
                break;
            }
        }
        sent.iter().any(|(known, _)| known == text)
    }

    /// Remove old strips so the directory does not grow forever.
    fn cleanup_comics(&self, keep: usize) {
        let dir = self.config.comics_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("png") {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name == "latest.png" {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            files.push((modified, path));
        }
        if files.len() <= keep {
            return;
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, path) in files.iter().take(files.len() - keep) {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Look up the picture of a user.
    ///
    /// The pictures are stored under the stable user id, so a user keeps its
    /// picture when the name changes. Assignments of this session win over the
    /// ones from the config file, the default picture is used when nothing was
    /// assigned.
    pub fn avatar_for(&self, user_id: &str) -> Option<PathBuf> {
        let login = {
            let state = self.state.lock().unwrap();
            if let Some(path) = state.avatar_overrides.get(user_id) {
                return Some(path.clone());
            }
            state
                .identities
                .get(user_id)
                .map(|identity| identity.login.clone())
        };
        if let Some(path) = self.config.avatar_for(user_id) {
            return Some(path.to_path_buf());
        }
        // Config files that were written before the user ids were used are
        // still read with the login.
        login
            .and_then(|login| self.config.avatar_for(&login))
            .map(|p| p.to_path_buf())
    }

    /// Give a chatter without a picture a random one from the pool.
    ///
    /// Avatars listed in `mod_avatars` are reserved and never picked here.
    /// Pictures that are already assigned to somebody else are preferred to be
    /// left alone, but they are reused when the pool runs out.
    fn auto_avatar(&self, user_id: &str) -> Option<PathBuf> {
        let pool = avatars::scan_pool(&self.config.avatars_dir);
        if pool.is_empty() {
            return None;
        }
        let reserved: HashSet<String> = self
            .config
            .mod_avatars
            .iter()
            .map(|name| name.to_lowercase())
            .collect();
        let mut candidates: Vec<&avatars::PoolEntry> = pool
            .iter()
            .filter(|entry| {
                let stem = entry.stem.to_lowercase();
                let file = entry
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                !reserved.contains(&stem) && !reserved.contains(&file)
            })
            .collect();
        if candidates.is_empty() {
            return None;
        }

        let used: HashSet<PathBuf> = {
            let state = self.state.lock().unwrap();
            state
                .avatar_overrides
                .values()
                .cloned()
                .chain(self.config.avatars.values().cloned())
                .collect()
        };
        let unused: Vec<&avatars::PoolEntry> = candidates
            .iter()
            .copied()
            .filter(|entry| !used.contains(&entry.path))
            .collect();
        if !unused.is_empty() {
            candidates = unused;
        }

        // No random crate needed: the user id and the current time seed a
        // stable hash, which is enough to spread people over the pool.
        let mut hasher = DefaultHasher::new();
        user_id.hash(&mut hasher);
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
            .hash(&mut hasher);
        let index = hasher.finish() as usize % candidates.len();
        let entry = candidates[index];

        if let Err(err) = self.assign_avatar(user_id, &entry.path) {
            eprintln!("avatar: could not save random assignment for {user_id}: {err}");
        }
        Some(entry.path.clone())
    }

    /// Find the user id of a name from the chat.
    ///
    /// The name may be the login, the display name or a part of it.
    pub fn resolve_user(&self, name: &str) -> Option<Identity> {
        let wanted = name.trim().trim_start_matches('@').to_lowercase();
        if wanted.is_empty() {
            return None;
        }
        let state = self.state.lock().unwrap();

        // The exact login.
        if let Some(user_id) = state.by_login.get(&wanted) {
            return state.identities.get(user_id).cloned();
        }
        // The exact display name.
        if let Some(user_id) = state.by_display_name.get(&wanted) {
            return state.identities.get(user_id).cloned();
        }
        // The same, but for the chatters of the channel.
        for identity in state.roster.values() {
            if identity.login.to_lowercase() == wanted
                || identity.display_name.to_lowercase() == wanted
            {
                return Some(identity.clone());
            }
        }
        // A part of the display name, but only if it is unambiguous.
        let mut found: Option<Identity> = None;
        for identity in state
            .identities
            .values()
            .chain(state.roster.values())
        {
            let display = identity.display_name.to_lowercase();
            if display.contains(&wanted) || identity.login.to_lowercase().contains(&wanted) {
                if found.is_some() {
                    return None;
                }
                found = Some(identity.clone());
            }
        }
        found
    }

    /// Store an avatar assignment and persist it in the config file.
    fn assign_avatar(&self, user_id: &str, path: &Path) -> anyhow::Result<()> {
        {
            let mut state = self.state.lock().unwrap();
            state
                .avatar_overrides
                .insert(user_id.to_string(), path.to_path_buf());
            // Panels that were drawn before are updated as well.
            for message in state
                .cells
                .iter_mut()
                .flat_map(|cell| cell.messages.iter_mut())
            {
                if message.user_id == user_id {
                    message.avatar = Some(path.to_path_buf());
                }
            }
            if let Some(participant) = state.participants.get_mut(user_id) {
                participant.avatar = Some(path.to_path_buf());
            }
        }
        self.persist_avatar(user_id, Some(path))
    }

    /// Remove an avatar assignment and persist the change.
    fn clear_avatar(&self, user_id: &str) -> anyhow::Result<bool> {
        let removed = {
            let mut state = self.state.lock().unwrap();
            state.avatar_overrides.remove(user_id)
        };
        self.persist_avatar(user_id, None)?;
        Ok(removed.is_some())
    }

    /// Write an avatar assignment into the config file.
    fn persist_avatar(&self, user_id: &str, path: Option<&Path>) -> anyhow::Result<()> {
        let mut config = Config::load_or_create(&self.config.path)?;
        match path {
            Some(path) => config.set_avatar(user_id, path),
            None => {
                config.clear_avatar(user_id);
            }
        }
        config.save(&self.config.path)
    }

    /// Change the background used by every cell and persist it.
    fn set_background(&self, path: &Path) -> anyhow::Result<()> {
        {
            let mut state = self.state.lock().unwrap();
            state.background = Some(path.to_path_buf());
            state.dirty = true;
        }
        self.persist_background(path)
    }

    fn persist_background(&self, path: &Path) -> anyhow::Result<()> {
        let mut config = Config::load_or_create(&self.config.path)?;
        config.background = Some(path.to_path_buf());
        config.save(&self.config.path)
    }

    fn current_background(&self) -> Option<PathBuf> {
        self.state.lock().unwrap().background.clone()
    }

    /// Handle an incoming chat message: update state, run commands and
    /// maybe render a comic.
    pub fn on_chat(&self, message: &ChatMessage, client: &IrcClient) {
        // Only handle messages of the configured channel.
        if !message.channel.eq_ignore_ascii_case(&self.config.channel) {
            return;
        }
        let login = avatars::normalize_login(&message.login);
        // The user id is stable, the name may change at any time.
        let user_id = if message.user_id.is_empty() {
            login.clone()
        } else {
            message.user_id.clone()
        };
        let bot_user_id = {
            let state = self.state.lock().unwrap();
            state.bot_user_id.clone()
        };
        // The bot may run on the same account as the person chatting. Only
        // the messages the bot sent itself are skipped, everything else of
        // that account is a normal chat message.
        if !bot_user_id.is_empty()
            && bot_user_id == user_id
            && self.sent_by_bot(&message.text)
        {
            return;
        }
        eprintln!("chat: {}: {}", message.display_name, message.text);

        // Commands and the answers of the bot are not part of the conversation.
        let is_command = message.text.trim_start().starts_with('!')
            || message.text.trim_start().starts_with("BOT:");
        // Look the picture up before the state is locked, the lookup needs the
        // lock itself. A chatter without an assigned picture gets a random one
        // the first time they write a normal message.
        let avatar = match self.avatar_for(&user_id) {
            Some(path) => Some(path),
            None => {
                // Older config files key the assignment by login; on the very
                // first message the identity map is still empty.
                let by_login = self.config.avatar_for(&login).map(|p| p.to_path_buf());
                match by_login {
                    Some(path) => Some(path),
                    None if !is_command => self.auto_avatar(&user_id),
                    None => None,
                }
            }
        };

        {
            let mut state = self.state.lock().unwrap();
            // Twitch repeats messages after a reconnect; ignore repeats.
            if !message.id.is_empty() && !state.seen_ids.insert(message.id.clone()) {
                return;
            }
            if state.seen_ids.len() > 4096 {
                state.seen_ids.clear();
            }
            // Remember how this user is called now.
            state.identities.insert(
                user_id.clone(),
                Identity {
                    user_id: user_id.clone(),
                    login: login.clone(),
                    display_name: message.display_name.clone(),
                },
            );
            state.by_login.insert(login.clone(), user_id.clone());
            state
                .by_display_name
                .insert(message.display_name.to_lowercase(), user_id.clone());
            if !message.display_name.is_empty() {
                state.by_display_name.insert(
                    message.display_name.to_lowercase(),
                    user_id.clone(),
                );
            }

            let entry = state
                .participants
                .entry(user_id.clone())
                .or_insert_with(|| Participant {
                    display_name: message.display_name.clone(),
                    color: message.color,
                    badges: message.badges.clone(),
                    avatar: None,
                    message_count: 0,
                    is_mod: message.is_mod,
                    is_sub: message.is_sub,
                });
            entry.display_name = message.display_name.clone();
            entry.color = message.color;
            entry.badges = message.badges.clone();
            entry.is_mod = message.is_mod;
            entry.is_sub = message.is_sub;
            entry.message_count += 1;
            entry.avatar = avatar.clone();

            let comic_message = ComicMessage {
                user_id: user_id.clone(),
                display_name: message.display_name.clone(),
                text: message.text.clone(),
                color: message.color,
                avatar,
                badges: message.badges.clone(),
                timestamp_ms: message.timestamp_ms,
                reply: message.reply_to.is_some(),
            };
            if !is_command {
                let window_ms = self.config.comic.cell_window_ms();
                let max_cells = self.config.comic.max_cells();
                state.push_message(comic_message, window_ms, max_cells);
                state.dirty = true;
            }
        }

        if self.handle_command(message, client) {
            return;
        }

        if self.config.render_mode == RenderMode::Each {
            self.maybe_render();
        }
    }

    /// May the sender change the pictures of people?
    ///
    /// Moderators, the broadcaster of the channel and the owner of the
    /// channel may do it. Somebody may always change their own picture.
    fn may_change_pictures(&self, message: &ChatMessage, user_id: &str) -> bool {
        let is_broadcaster = message.badges.iter().any(|badge| badge == "broadcaster");
        let is_owner = message.login.eq_ignore_ascii_case(&self.config.channel);
        message.is_mod || is_broadcaster || is_owner || user_id.is_empty()
    }

    /// May the sender change the background of the comic?
    fn may_change_background(&self, message: &ChatMessage) -> bool {
        let is_broadcaster = message.badges.iter().any(|badge| badge == "broadcaster");
        let is_owner = message.login.eq_ignore_ascii_case(&self.config.channel);
        message.is_mod || is_broadcaster || is_owner
    }

    /// Handle a chat command. Returns true when the message was a command.
    fn handle_command(&self, message: &ChatMessage, client: &IrcClient) -> bool {
        let text = message.text.trim();
        let Some(command) = text.strip_prefix('!') else {
            return false;
        };
        let mut parts = command.split_whitespace();
        let name = parts.next().unwrap_or("").to_ascii_lowercase();
        let args: Vec<&str> = parts.collect();
        let user_id = if message.user_id.is_empty() {
            avatars::normalize_login(&message.login)
        } else {
            message.user_id.clone()
        };

        match name.as_str() {
            "help" | "commands" => {
                self.send(
                    client,
                    "!comic (render now) | !avatar / !avatars (pictures) | !setavatar <name> <picture> | !clearavatar <name> | !background / !backgrounds (cell background) | !setbackground <picture> | !whois <name>",
                );
            }
            "avatar" => {
                match self.avatar_for(&user_id) {
                    Some(path) => self.send(client, &format!(
                        "your comic picture is {}",
                        path.file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("unknown")
                    )),
                    None => self.send(client, "you have no comic picture yet"),
                }
            }
            "avatars" => {
                let pool = avatars::scan_pool(&self.config.avatars_dir);
                if pool.is_empty() {
                    self.send(
                        client,
                        &format!(
                            "no pictures found in {}",
                            self.config.avatars_dir.display()
                        ),
                    );
                } else {
                    let names: Vec<String> = pool
                        .iter()
                        .map(|entry| entry.stem.clone())
                        .collect();
                    self.send(
                        client,
                        &format!("available pictures: {}", names.join(", ")),
                    );
                }
            }
            "background" => match self.current_background() {
                Some(path) => self.send(
                    client,
                    &format!(
                        "the cell background is {}",
                        path.file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("unknown")
                    ),
                ),
                None => self.send(client, "no cell background is set"),
            },
            "backgrounds" => {
                let pool = avatars::scan_pool(&self.config.backgrounds_dir);
                if pool.is_empty() {
                    self.send(
                        client,
                        &format!(
                            "no backgrounds found in {}",
                            self.config.backgrounds_dir.display()
                        ),
                    );
                } else {
                    let names: Vec<String> =
                        pool.iter().map(|entry| entry.stem.clone()).collect();
                    self.send(
                        client,
                        &format!("available backgrounds: {}", names.join(", ")),
                    );
                }
            }
            "setbackground" => {
                if args.is_empty() {
                    self.send(client, "usage: !setbackground <picture>");
                    return true;
                }
                if !self.may_change_background(message) {
                    self.send(client, "only moderators can change the background");
                    return true;
                }
                let pool = avatars::scan_pool(&self.config.backgrounds_dir);
                let Some(entry) = avatars::find_in_pool(&pool, args[0]) else {
                    let names: Vec<String> = pool.iter().map(|e| e.stem.clone()).collect();
                    self.send(
                        client,
                        &format!(
                            "no background named '{}'. available: {}",
                            args[0],
                            names.join(", ")
                        ),
                    );
                    return true;
                };
                match self.set_background(&entry.path) {
                    Ok(()) => self.send(
                        client,
                        &format!("cells now use the background {}", entry.stem),
                    ),
                    Err(err) => self.send(
                        client,
                        &format!("could not save the config: {err}"),
                    ),
                }
            }
            "setavatar" => {
                if args.len() < 2 {
                    self.send(client, "usage: !setavatar <name> <picture>");
                    return true;
                }
                let Some(target) = self.resolve_user(args[0]) else {
                    self.send(
                        client,
                        &format!("I don't know anybody called '{}'", args[0]),
                    );
                    return true;
                };
                if !self.may_change_pictures(message, &target.user_id) {
                    self.send(
                        client,
                        "only moderators can change the pictures of other people",
                    );
                    return true;
                }
                let pool = avatars::scan_pool(&self.config.avatars_dir);
                let Some(entry) = avatars::find_in_pool(&pool, args[1]) else {
                    let names: Vec<String> = pool.iter().map(|e| e.stem.clone()).collect();
                    self.send(
                        client,
                        &format!(
                            "no picture named '{}'. available: {}",
                            args[1],
                            names.join(", ")
                        ),
                    );
                    return true;
                };
                match self.assign_avatar(&target.user_id, &entry.path) {
                    Ok(()) => self.send(
                        client,
                        &format!("{} now uses {}", target.display_name, entry.stem),
                    ),
                    Err(err) => {
                        self.send(client, &format!("could not save the config: {err}"))
                    }
                }
            }
            "clearavatar" => {
                if args.is_empty() {
                    self.send(client, "usage: !clearavatar <name>");
                    return true;
                }
                let Some(target) = self.resolve_user(args[0]) else {
                    self.send(
                        client,
                        &format!("I don't know anybody called '{}'", args[0]),
                    );
                    return true;
                };
                if !self.may_change_pictures(message, &target.user_id) {
                    self.send(
                        client,
                        "only moderators can change the pictures of other people",
                    );
                    return true;
                }
                match self.clear_avatar(&target.user_id) {
                    Ok(true) => self.send(
                        client,
                        &format!("removed the picture of {}", target.display_name),
                    ),
                    Ok(false) => self.send(client, &format!(
                        "{} had no picture",
                        target.display_name
                    )),
                    Err(err) => {
                        self.send(client, &format!("could not save the config: {err}"))
                    }
                }
            }
            "whois" => {
                if args.is_empty() {
                    self.send(client, "usage: !whois <name>");
                    return true;
                }
                let Some(target) = self.resolve_user(args[0]) else {
                    self.send(
                        client,
                        &format!("I don't know anybody called '{}'", args[0]),
                    );
                    return true;
                };
                match self.avatar_for(&target.user_id) {
                    Some(path) => self.send(
                        client,
                        &format!(
                            "{} uses {}",
                            target.display_name,
                            path.file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("unknown")
                        ),
                    ),
                    None => self.send(client, &format!(
                        "{} has no picture",
                        target.display_name
                    )),
                }
            }
            "comic" => {
                self.render_and_announce(client);
            }
            _ => {
                return false;
            }
        }
        true
    }

    /// Render the comic when the configured interval has passed.
    ///
    /// The bot stays quiet in the chat, the comic only goes to the window and
    /// the archive directory.
    fn maybe_render(&self) {
        {
            let state = self.state.lock().unwrap();
            if !state.dirty || state.cells.is_empty() {
                return;
            }
            if let Some(last) = state.last_render {
                if last.elapsed() < Duration::from_secs(self.config.comic.min_interval_secs) {
                    return;
                }
            }
        }
        let _ = self.render_current();
    }

    /// Periodic hook for the main loop.
    ///
    /// Rendering is also triggered after a message, but a burst of messages
    /// may still be running when the interval has passed. The tick makes sure
    /// the final state of the cell reaches the overlay without waiting for
    /// another message.
    pub fn tick(&self) {
        if self.config.render_mode != RenderMode::Each {
            return;
        }
        self.maybe_render();
    }

    /// Render the recent messages and tell the chat about it.
    ///
    /// Only used by the `!comic` command.
    pub fn render_and_announce(&self, client: &IrcClient) -> Option<PathBuf> {
        let path = self.render_current().ok()?;
        self.send(client, "the comic is ready");
        Some(path)
    }

    /// Render the recent messages into a PNG inside the comics directory.
    pub fn render_current(&self) -> anyhow::Result<PathBuf> {
        let (cells, roster, title, subtitle, background) = {
            let state = self.state.lock().unwrap();
            let mut cells: Vec<Cell> = state.cells.iter().cloned().collect();
            let max = self.config.comic.max_cells();
            if cells.len() > max {
                cells = cells.split_off(cells.len() - max);
            }
            let message_count: usize = cells.iter().map(|cell| cell.messages.len()).sum();
            let background = state.background.clone();
            // The roster shows everybody who took part in the conversation.
            let roster: Vec<(String, Color, Option<PathBuf>, u32)> = state
                .participants
                .values()
                .map(|participant| {
                    (
                        participant.display_name.clone(),
                        participant.color,
                        participant.avatar.clone(),
                        participant.message_count,
                    )
                })
                .collect();
            let title = format!("#{}", self.config.channel);
            let subtitle = format!(
                "{} message{} in {} cell{} · {}",
                message_count,
                if message_count == 1 { "" } else { "s" },
                cells.len(),
                if cells.len() == 1 { "" } else { "s" },
                timestamp_label()
            );
            (cells, roster, title, subtitle, background)
        };

        // The picture lookup needs the lock, so it happens outside of it.
        let mut characters: Vec<Character> = roster
            .into_iter()
            .map(|(display_name, color, avatar, messages)| Character {
                display_name: display_name.clone(),
                color,
                avatar: avatar.or_else(|| self.avatar_for(&display_name)),
                messages,
            })
            .collect();
        characters.sort_by(|a, b| b.messages.cmp(&a.messages));
        let page = ComicPage {
            cells,
            characters,
            background,
        };

        let image = comic::render(
            &page,
            &self.fonts,
            &self.config.comic,
            &title,
            &subtitle,
        );

        let dir = self.config.comics_dir();
        std::fs::create_dir_all(&dir)?;
        let stamp = file_stamp();
        let path = dir.join(format!("comic-{stamp}.png"));
        image.save(&path)?;

        // Keep a stable name for the archive and external scripts.
        std::fs::copy(&path, dir.join("latest.png"))?;

        if let Some(display) = &self.display {
            display.show(image);
        }

        self.cleanup_comics(200);

        let mut state = self.state.lock().unwrap();
        state.last_render = Some(Instant::now());
        state.dirty = false;
        state.comic_count += 1;
        state.last_comic = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string());
        Ok(path)
    }

    /// Summary of the current state, used by the status endpoint.
    pub fn status_json(&self) -> String {
        let state = self.state.lock().unwrap();
        let participants: Vec<serde_json::Value> = state
            .participants
            .iter()
            .map(|(login, participant)| {
                serde_json::json!({
                    "login": login,
                    "display_name": participant.display_name,
                    "badges": participant.badges,
                    "messages": participant.message_count,
                    "moderator": participant.is_mod,
                    "subscriber": participant.is_sub,
                    "avatar": participant.avatar.as_ref().map(|p| p.to_string_lossy().to_string()),
                })
            })
            .collect();
        serde_json::json!({
            "channel": self.config.channel,
            "comic_count": state.comic_count,
            "cells": state.cells.len(),
            "messages": state.message_count(),
            "last_comic": state.last_comic,
            "render_mode": self.config.render_mode,
            "participants": participants,
        })
        .to_string()
    }
}

/// Build the HTTP handler for the OAuth callback and the status endpoint.
pub fn http_handler(
    bot: Arc<Bot>,
    auth_events: mpsc::UnboundedSender<AuthEvent>,
    authorize_url: String,
    expected_state: String,
) -> Handler {
    Arc::new(move |request: HttpRequest| -> HandlerFuture {
        let bot = bot.clone();
        let auth_events = auth_events.clone();
        let authorize_url = authorize_url.clone();
        let expected_state = expected_state.clone();
        Box::pin(async move {
            handle_request(
                bot,
                auth_events,
                authorize_url,
                expected_state,
                request,
            )
            .await
        })
    })
}

async fn handle_request(
    bot: Arc<Bot>,
    auth_events: mpsc::UnboundedSender<AuthEvent>,
    authorize_url: String,
    expected_state: String,
    request: HttpRequest,
) -> HttpResponse {
    if !request.method.eq_ignore_ascii_case("GET") {
        return HttpResponse::method_not_allowed();
    }

    // The redirect URI is "http://localhost:3000" without a path, so Twitch
    // sends the callback to "/" (with the code in the query string). Detect
    // the OAuth parameters on every path, not only on "/callback".
    if request.query("code").is_some() || request.query("error").is_some() {
        return handle_callback(&auth_events, &expected_state, &request);
    }

    match request.path.as_str() {
        "/" => HttpResponse::html(
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>comicbot</title></head>\
             <body style=\"font-family:sans-serif;padding:3rem\">\
             <h2>comicbot is running</h2>\
             <p>The comic is shown in its own window.</p>\
             </body></html>",
        ),
        "/auth" => HttpResponse::redirect(&authorize_url),
        "/callback" => {
            // Redirect URI with a path, but without any OAuth parameters.
            let _ = auth_events.send(AuthEvent::Failed(
                "callback without an authorization code".to_string(),
            ));
            HttpResponse::html(
                "<!doctype html><html><body style=\"font-family:sans-serif;padding:3rem\">\
                 <h2>Login failed</h2><p>No authorization code was present in the callback.</p>\
                 </body></html>",
            )
        }
        "/api/status" => HttpResponse::json(bot.status_json()),
        _ => HttpResponse::not_found("not found"),
    }
}

/// Handle the OAuth redirect.
fn handle_callback(
    auth_events: &mpsc::UnboundedSender<AuthEvent>,
    expected_state: &str,
    request: &crate::server::HttpRequest,
) -> HttpResponse {
    // The user may have denied the access.
    if let Some(error) = request.query("error") {
        let description = request
            .query("error_description")
            .unwrap_or("the request was not approved");
        let _ = auth_events.send(AuthEvent::Failed(format!("{error}: {description}")));
        return HttpResponse::html(
            "<!doctype html><html><body style=\"font-family:sans-serif;padding:3rem\">\
             <h2>Login cancelled</h2><p>The authorization was not approved. \
             You can close this window.</p></body></html>",
        );
    }

    // The state has to match the one we sent with the authorize URL.
    match request.query("state") {
        Some(state) if state == expected_state => {}
        _ => {
            let _ = auth_events
                .send(AuthEvent::Failed("callback with a wrong state".to_string()));
            return HttpResponse::html(
                "<!doctype html><html><body style=\"font-family:sans-serif;padding:3rem\">\
                 <h2>Login failed</h2><p>The login request could not be verified. \
                 Please start the login again.</p></body></html>",
            );
        }
    }

    match request.query("code") {
        Some(code) if !code.is_empty() => {
            let _ = auth_events.send(AuthEvent::Code(code.to_string()));
            HttpResponse::html(
                "<!doctype html><html><body style=\"font-family:sans-serif;padding:3rem\">\
                 <h2>Login received</h2><p>You can close this window and go back to the terminal.</p>\
                 </body></html>",
            )
        }
        _ => {
            let _ = auth_events.send(AuthEvent::Failed(
                "callback without code".to_string(),
            ));
            HttpResponse::html(
                "<!doctype html><html><body style=\"font-family:sans-serif;padding:3rem\">\
                 <h2>Login failed</h2><p>No authorization code was present in the callback.</p>\
                 </body></html>",
            )
        }
    }
}

/// Timestamp used in file names.
fn file_stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Simple UTC stamp, good enough to keep the files sortable.
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
}

/// Human readable timestamp for the comic header.
fn timestamp_label() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hour, minute) = (rem / 3600, (rem % 3600) / 60);
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC")
}

/// Convert days since the epoch into a civil date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_date_conversion() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
    }

    #[test]
    fn stamps_are_ordered() {
        assert!(file_stamp().len() == 15);
    }

    /// The bot often runs on the same account as the person chatting. Those
    /// messages have to be processed, only the answers of the bot itself are
    /// skipped.
    #[test]
    fn own_account_messages_are_not_dropped() {
        let dir = std::env::temp_dir().join(format!(
            "comicbot-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("comicbot.toml");
        let mut config = Config::load_or_create(&config_path).unwrap();
        config.render_mode = crate::config::RenderMode::Off;
        let fonts = crate::fonts::Fonts::load(&config.fonts).unwrap();
        let bot = Bot::new(config, fonts);
        {
            let mut state = bot.state.lock().unwrap();
            state.bot_login = "salmon85".to_string();
        }

        let (client, _outgoing) = crate::irc::IrcClient::for_test("salmon85");

        let mut message = ChatMessage {
            id: "one".to_string(),
            channel: "salmon85".to_string(),
            user_id: "4242".to_string(),
            login: "salmon85".to_string(),
            display_name: "Salmon85".to_string(),
            text: "hello from my own account".to_string(),
            color: Color::rgb(0, 173, 255),
            badges: vec![],
            is_mod: true,
            is_sub: false,
            timestamp_ms: 1,
            reply_to: None,
        };

        bot.on_chat(&message, &client);
        assert_eq!(
            bot.state.lock().unwrap().message_count(),
            1,
            "a message of the own account must reach the comic"
        );

        // The answer of the bot must not trigger a second comic.
        bot.send(&client, "the comic is ready");
        message.id = "two".to_string();
        message.text = "BOT: the comic is ready".to_string();
        bot.on_chat(&message, &client);
        assert_eq!(
            bot.state.lock().unwrap().message_count(),
            1,
            "the bot must not answer itself"
        );

        std::fs::remove_dir_all(dir).ok();
    }

    /// A picture belongs to the user, not to the name. When somebody changes
    /// the name the picture stays.
    #[test]
    fn pictures_follow_the_user_id() {
        let dir = std::env::temp_dir().join(format!(
            "comicbot-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let picture = dir.join("hero.png");
        std::fs::write(&picture, b"not really a png").unwrap();

        let config_path = dir.join("comicbot.toml");
        let mut config = Config::load_or_create(&config_path).unwrap();
        config.render_mode = crate::config::RenderMode::Off;
        let fonts = crate::fonts::Fonts::load(&config.fonts).unwrap();
        let bot = Bot::new(config, fonts);
        let (client, _outgoing) = crate::irc::IrcClient::for_test("salmon85");

        let message = ChatMessage {
            id: "one".to_string(),
            channel: "salmon85".to_string(),
            user_id: "777".to_string(),
            login: "oldname".to_string(),
            display_name: "OldName".to_string(),
            text: "hello".to_string(),
            color: Color::rgb(0, 173, 255),
            badges: vec![],
            is_mod: true,
            is_sub: false,
            timestamp_ms: 1,
            reply_to: None,
        };
        bot.on_chat(&message, &client);

        // The name of the user can be used to find them.
        let identity = bot.resolve_user("oldname").expect("the user is known");
        assert_eq!(identity.user_id, "777");

        bot.assign_avatar(&identity.user_id, &picture).unwrap();
        assert_eq!(bot.avatar_for("777"), Some(picture.clone()));

        // The user changes the name, the picture stays.
        let renamed = ChatMessage {
            id: "two".to_string(),
            user_id: "777".to_string(),
            login: "newname".to_string(),
            display_name: "NewName".to_string(),
            ..message.clone()
        };
        bot.on_chat(&renamed, &client);
        assert_eq!(bot.avatar_for("777"), Some(picture.clone()));
        assert_eq!(
            bot.resolve_user("newname").map(|identity| identity.user_id),
            Some("777".to_string())
        );

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn avatar_assignments_are_persisted() {
        let dir = std::env::temp_dir().join(format!(
            "comicbot-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("avatars")).unwrap();
        let picture = dir.join("avatars").join("hero.png");
        std::fs::write(&picture, b"not really a png").unwrap();

        let config_path = dir.join("comicbot.toml");
        let config = Config::load_or_create(&config_path).unwrap();
        let fonts = crate::fonts::Fonts::load(&config.fonts).unwrap();
        let bot = Bot::new(config, fonts);

        assert!(bot.avatar_for("4242").is_none());
        bot.assign_avatar("4242", &picture).unwrap();
        assert_eq!(bot.avatar_for("4242"), Some(picture.clone()));

        // The assignment survives a restart because it is written to the file.
        let reloaded = Config::load_or_create(&config_path).unwrap();
        assert_eq!(
            reloaded.avatar_for("4242"),
            Some(picture.clone().as_path()),
            "assignment should be stored in the config file"
        );

        assert!(bot.clear_avatar("4242").unwrap());
        assert!(bot.avatar_for("4242").is_none());

        std::fs::remove_dir_all(dir).ok();
    }

    /// Messages in one second share a cell, and the page keeps only
    /// `columns * rows` cells.
    #[test]
    fn messages_are_grouped_into_cells_and_old_cells_drop() {
        let dir = std::env::temp_dir().join(format!(
            "comicbot-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("comicbot.toml");
        let mut config = Config::load_or_create(&config_path).unwrap();
        config.render_mode = crate::config::RenderMode::Off;
        config.comic.columns = 3;
        config.comic.rows = 2;
        config.comic.cell_window_secs = 1.0;
        let fonts = crate::fonts::Fonts::load(&config.fonts).unwrap();
        let bot = Bot::new(config, fonts);
        let (client, _outgoing) = crate::irc::IrcClient::for_test("salmon85");

        let message = |id: &str, timestamp_ms: i64| ChatMessage {
            id: id.to_string(),
            channel: "salmon85".to_string(),
            user_id: format!("user-{id}"),
            login: "tester".to_string(),
            display_name: "Tester".to_string(),
            text: "hello".to_string(),
            color: Color::rgb(0, 173, 255),
            badges: vec![],
            is_mod: false,
            is_sub: false,
            timestamp_ms,
            reply_to: None,
        };

        for (index, timestamp) in [0, 300, 900].iter().enumerate() {
            let mut chat = message(&format!("a{index}"), *timestamp);
            chat.user_id = "alice".to_string();
            chat.login = "alice".to_string();
            chat.display_name = "Alice".to_string();
            bot.on_chat(&chat, &client);
        }
        {
            let state = bot.state.lock().unwrap();
            assert_eq!(state.cells.len(), 1, "one second of chat is one cell");
            assert_eq!(state.cells[0].messages.len(), 3);
        }

        for index in 0..6 {
            bot.on_chat(&message(&format!("b{index}"), 2000 + index * 1000), &client);
        }
        let state = bot.state.lock().unwrap();
        assert_eq!(state.cells.len(), 6, "the page holds columns * rows cells");
        assert_eq!(
            state.cells.front().unwrap().first_time_ms,
            2000,
            "the oldest cell is dropped"
        );
        assert_eq!(state.cells.back().unwrap().first_time_ms, 7000);
        drop(state);

        std::fs::remove_dir_all(dir).ok();
    }

    /// A background picked from the pool is stored in the running state and
    /// written back to the config file.
    #[test]
    fn background_is_saved_to_the_config() {
        let dir = std::env::temp_dir().join(format!(
            "comicbot-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("backgrounds")).unwrap();
        let picture = dir.join("backgrounds").join("paper.png");
        std::fs::write(&picture, b"not really a png").unwrap();

        let config_path = dir.join("comicbot.toml");
        let mut config = Config::load_or_create(&config_path).unwrap();
        config.render_mode = crate::config::RenderMode::Off;
        let fonts = crate::fonts::Fonts::load(&config.fonts).unwrap();
        let bot = Bot::new(config, fonts);
        let (client, _outgoing) = crate::irc::IrcClient::for_test("salmon85");

        let message = ChatMessage {
            id: "background".to_string(),
            channel: "salmon85".to_string(),
            user_id: "1".to_string(),
            login: "salmon85".to_string(),
            display_name: "Salmon85".to_string(),
            text: "!setbackground paper".to_string(),
            color: Color::rgb(0, 173, 255),
            badges: vec![],
            is_mod: true,
            is_sub: false,
            timestamp_ms: 1,
            reply_to: None,
        };
        bot.on_chat(&message, &client);

        assert_eq!(
            bot.state.lock().unwrap().background.as_deref(),
            Some(picture.as_path())
        );
        let reloaded = Config::load_or_create(&config_path).unwrap();
        assert_eq!(reloaded.background.as_deref(), Some(picture.as_path()));

        std::fs::remove_dir_all(dir).ok();
    }

    /// A chatter without an assigned picture gets an unused random avatar,
    /// but avatars reserved for moderators are skipped.
    #[test]
    fn random_avatar_skips_reserved_avatars() {
        let dir = std::env::temp_dir().join(format!(
            "comicbot-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("avatars")).unwrap();
        let normal = dir.join("avatars").join("hero.png");
        let reserved = dir.join("avatars").join("mod.png");
        std::fs::write(&normal, b"not really a png").unwrap();
        std::fs::write(&reserved, b"not really a png").unwrap();

        let config_path = dir.join("comicbot.toml");
        let mut config = Config::load_or_create(&config_path).unwrap();
        config.render_mode = crate::config::RenderMode::Off;
        config.mod_avatars = vec!["mod".to_string()];
        let fonts = crate::fonts::Fonts::load(&config.fonts).unwrap();
        let bot = Bot::new(config, fonts);
        let (client, _outgoing) = crate::irc::IrcClient::for_test("salmon85");

        let message = ChatMessage {
            id: "random".to_string(),
            channel: "salmon85".to_string(),
            user_id: "777".to_string(),
            login: "newbie".to_string(),
            display_name: "Newbie".to_string(),
            text: "hello".to_string(),
            color: Color::rgb(0, 173, 255),
            badges: vec![],
            is_mod: false,
            is_sub: false,
            timestamp_ms: 1,
            reply_to: None,
        };
        bot.on_chat(&message, &client);

        assert_eq!(bot.avatar_for("777"), Some(normal.clone()));
        let reloaded = Config::load_or_create(&config_path).unwrap();
        assert_eq!(reloaded.avatar_for("777"), Some(normal.as_path()));

        std::fs::remove_dir_all(dir).ok();
    }
}
