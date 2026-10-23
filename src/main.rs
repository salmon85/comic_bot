//! comicbot - a Twitch chat bot that renders conversations as comic strips.
//!
//! The layout follows Microsoft Comic Chat: every chatter is a character with
//! a picture and every message becomes a speech bubble.

mod avatars;
mod auth;
mod bot;
mod comic;
mod config;
mod display;
mod fonts;
mod https;
mod irc;
mod render;
mod server;

use anyhow::{Context, Result};
use bot::{AuthEvent, Bot};
use comic::{Cell, Character, ComicMessage, ComicPage};
use config::Config;
use render::Color;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut config_path = PathBuf::from("comicbot.toml");
    let mut rest: Vec<String> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--config" | "-c" => {
                index += 1;
                let value = args
                    .get(index)
                    .context("--config needs a path")?
                    .clone();
                config_path = PathBuf::from(value);
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            "--version" | "-V" => {
                println!("comicbot {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            other => rest.push(other.to_string()),
        }
        index += 1;
    }

    let command = rest.first().cloned().unwrap_or_else(|| "run".to_string());
    let args = &rest[rest.len().min(1)..];

    match command.as_str() {
        "run" => run(&config_path).await,
        "login" => login(&config_path, args).await,
        "whoami" => whoami(&config_path).await,
        "app-token" => app_token(&config_path).await,
        "logout" => logout(&config_path),
        "irc-check" => irc_check(&config_path).await,
        "say" => say(&config_path, args).await,
        "purge" => purge(&config_path, args).await,
        "render" => render_file(&config_path, args),
        "demo" => demo(&config_path),
        "help" => {
            print_help();
            Ok(())
        }
        other => {
            eprintln!("unknown command '{other}'\n");
            print_help();
            Ok(())
        }
    }
}

fn print_help() {
    println!(
        r#"comicbot - Twitch comic chat bot

USAGE:
    comicbot [OPTIONS] <COMMAND>

COMMANDS:
    run          Start the bot: connect to chat and show comics in a window
    login        Log in to Twitch (opens a browser, stores the token)
    whoami       Show which account the stored token belongs to
    app-token    Print an app access token
    logout       Delete the stored token
    irc-check    Test the Twitch IRC connection and print the reply
    say <text>   Send a message to the channel (handy for testing)
    purge        Delete the bot's own messages from the chat history
    render       Render a comic from a JSON transcript file
    demo         Render a demo comic with generated placeholder pictures

OPTIONS:
    -c, --config <PATH>   Config file (default: comicbot.toml)

Chat commands:
    !comic                  render the comic now
    !avatar                 show your own picture
    !avatars                list the available pictures
    !setavatar <u> <pic>   assign a picture to a chatter (moderators)
    !clearavatar <u>        remove the picture of a chatter (moderators)
    !background             show the current cell background
    !backgrounds            list the available backgrounds
    !setbackground <pic>    switch the cell background (moderators)
    !whois <u>              show the picture of a chatter"#
    );
}

/// Load the config and make sure the directories exist.
fn prepare(config_path: &Path) -> Result<Config> {
    let config = Config::load_or_create(config_path)?;
    std::fs::create_dir_all(&config.data_dir)?;
    std::fs::create_dir_all(&config.comics_dir())?;
    std::fs::create_dir_all(&config.avatars_dir)?;
    std::fs::create_dir_all(&config.backgrounds_dir)?;
    Ok(config)
}

/// Load a usable user token, refreshing it when needed.
async fn user_token(client: &https::HttpsClient, config: &Config) -> Result<Option<auth::TokenSet>> {
    if let Ok(env_token) = std::env::var("COMICBOT_TOKEN") {
        if !env_token.trim().is_empty() {
            return Ok(Some(auth::TokenSet {
                access_token: env_token.trim().to_string(),
                refresh_token: String::new(),
                expires_in: 0,
                scope: vec![],
                token_type: "bearer".to_string(),
                obtained_at: 0,
            }));
        }
    }

    let path = config.token_path();
    let Ok(mut token) = auth::load_token(&path) else {
        return Ok(None);
    };
    if token.is_expired() && !token.refresh_token.is_empty() {
        println!("token expired, refreshing...");
        let mut refreshed = auth::refresh(client, config, &token.refresh_token).await?;
        if refreshed.refresh_token.is_empty() {
            refreshed.refresh_token = token.refresh_token.clone();
        }
        token = refreshed;
        auth::store_token(&path, &token)?;
    }
    Ok(Some(token))
}

/// Resolve the Twitch login the bot should connect as.
///
/// Twitch requires the IRC nickname to match the account that owns the OAuth
/// token, so `bot_name` selects the account rather than renaming it.
fn configured_bot_login(config: &Config, identity: &auth::Identity) -> Result<String> {
    let Some(name) = config
        .bot_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    else {
        return Ok(identity.login.clone());
    };
    let name = name.trim_start_matches('@');
    if !name.eq_ignore_ascii_case(&identity.login) {
        anyhow::bail!(
            "bot_name is '{name}' but the stored token belongs to '{}'.\n\
             Twitch ties the IRC nickname to the OAuth token. Run 'comicbot logout' and \
             'comicbot login' with the bot account, or remove bot_name from the config.",
            identity.login
        );
    }
    Ok(identity.login.clone())
}

/// Force a token refresh after an authentication failure, falling back to a
/// fresh interactive login when no refresh token is available.
async fn refresh_or_login(
    client: &https::HttpsClient,
    config: &Config,
    auth_events: &mut mpsc::UnboundedReceiver<AuthEvent>,
    state: &str,
) -> Result<auth::TokenSet> {
    if std::env::var("COMICBOT_TOKEN")
        .map(|token| !token.trim().is_empty())
        .unwrap_or(false)
    {
        anyhow::bail!("COMICBOT_TOKEN was rejected; replace it and restart comicbot");
    }
    let path = config.token_path();
    if let Ok(token) = auth::load_token(&path) {
        if !token.refresh_token.is_empty() {
            match auth::refresh(client, config, &token.refresh_token).await {
                Ok(mut refreshed) => {
                    if refreshed.refresh_token.is_empty() {
                        refreshed.refresh_token = token.refresh_token.clone();
                    }
                    auth::store_token(&path, &refreshed)?;
                    return Ok(refreshed);
                }
                Err(err) => eprintln!("token refresh failed: {err}"),
            }
        }
    }
    login_interactively(client, config, auth_events, state, auth::BOT_SCOPES).await
}

/// Start the local HTTP server for the OAuth callback and status endpoint.
async fn start_server(
    bot: std::sync::Arc<Bot>,
    config: &Config,
    auth_events: mpsc::UnboundedSender<AuthEvent>,
) -> Result<String> {
    let state = auth::random_state();
    let authorize_url = auth::authorize_url(config, &state, auth::BOT_SCOPES);
    let handler = bot::http_handler(bot, auth_events, authorize_url.clone(), state.clone());

    // "localhost" resolves to ::1 on some systems and to 127.0.0.1 on others,
    // so bind both loopback families when the host is a loopback name.
    let hosts: Vec<String> = match config.http.host.as_str() {
        "localhost" | "127.0.0.1" => vec!["127.0.0.1".to_string(), "[::1]".to_string()],
        other => vec![other.to_string()],
    };

    let mut listeners = Vec::new();
    for host in hosts {
        let address = format!("{}:{}", host, config.http.port);
        match tokio::net::TcpListener::bind(&address).await {
            Ok(listener) => listeners.push(listener),
            Err(err) => eprintln!("warning: cannot bind {address}: {err}"),
        }
    }
    if listeners.is_empty() {
        anyhow::bail!(
            "cannot bind port {} on {}",
            config.http.port,
            config.http.host
        );
    }

    println!("http server:  {}/", config.http.base_url());
    println!("login page:   {}/auth", config.http.base_url());
    for listener in listeners {
        let handler = handler.clone();
        tokio::spawn(async move {
            if let Err(err) = server::serve(listener, handler).await {
                eprintln!("http server stopped: {err}");
            }
        });
    }
    Ok(state)
}

/// Open an authorization URL in the user's default browser.
fn open_browser(url: &str) {
    #[cfg(target_os = "linux")]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("rundll32.exe")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let result: std::io::Result<std::process::Child> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no browser opener for this platform",
    ));

    match result {
        Ok(_) => println!("opened the authorization page in your browser"),
        Err(err) => eprintln!(
            "could not open the browser automatically ({err}); open the URL above manually"
        ),
    }
}

/// Wait for the OAuth callback and exchange the code.
async fn login_interactively(
    client: &https::HttpsClient,
    config: &Config,
    auth_events: &mut mpsc::UnboundedReceiver<AuthEvent>,
    state: &str,
    scopes: &str,
) -> Result<auth::TokenSet> {
    let url = auth::authorize_url(config, state, scopes);
    println!();
    println!("Open this URL in your browser to authorize the bot:\n");
    println!("  {url}\n");
    open_browser(&url);
    println!("Waiting for the callback on {} ...", config.redirect_uri);

    loop {
        match auth_events.recv().await {
            Some(AuthEvent::Code(code)) => match auth::exchange_code(client, config, &code).await {
                Ok(token) => {
                    auth::store_token(&config.token_path(), &token)?;
                    println!("logged in, token stored in {}", config.token_path().display());
                    return Ok(token);
                }
                Err(err) => {
                    // The code was rejected: let the user simply try again.
                    eprintln!("login failed: {err}");
                    println!("open the URL above again to retry");
                }
            },
            Some(AuthEvent::Failed(reason)) => {
                eprintln!("login failed: {reason}");
            }
            None => anyhow::bail!("login server stopped unexpectedly"),
        }
    }
}

/// Resolve when the comic window closes, if a window was opened.
async fn wait_for_window_close(receiver: &mut Option<tokio::sync::oneshot::Receiver<()>>) {
    match receiver {
        Some(receiver) => {
            let _ = receiver.await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// `comicbot run`
async fn run(config_path: &Path) -> Result<()> {
    let config = prepare(config_path)?;
    let fonts = fonts::Fonts::load(&config.fonts)?;
    let client = https::HttpsClient::new()?;

    let mut bot = Bot::new(config.clone(), fonts);
    let mut window_closed = None;
    match display::ComicDisplay::spawn(config.comic.width as usize) {
        Some((comic_display, closed)) => {
            // Show the last strip while waiting for the first new message.
            if let Ok(reader) =
                image::ImageReader::open(config.comics_dir().join("latest.png"))
            {
                if let Ok(decoded) = reader.decode() {
                    comic_display.show(decoded.to_rgba8());
                }
            }
            bot.attach_display(comic_display);
            window_closed = Some(closed);
            println!("comic window ready");
        }
        None => eprintln!(
            "warning: no window display available; comics are still written to {}",
            config.comics_dir().display()
        ),
    }
    let bot = std::sync::Arc::new(bot);
    let (auth_tx, mut auth_rx) = mpsc::unbounded_channel::<AuthEvent>();
    let login_state = start_server(bot.clone(), &config, auth_tx).await?;

    let token = match user_token(&client, &config).await? {
        Some(token) => token,
        None => {
            let login = login_interactively(
                &client,
                &config,
                &mut auth_rx,
                &login_state,
                auth::BOT_SCOPES,
            );
            tokio::pin!(login);
            loop {
                tokio::select! {
                    result = &mut login => break result?,
                    _ = wait_for_window_close(&mut window_closed) => {
                        println!("\ncomic window closed");
                        return Ok(());
                    }
                }
            }
        }
    };

    let identity = auth::validate(&client, &token.access_token).await.map_err(|err| {
        anyhow::anyhow!(
            "the stored token was rejected by Twitch ({err}).\nRun 'comicbot login' to log in again."
        )
    })?;
    let bot_login = configured_bot_login(&config, &identity)?;
    if !identity.scopes.iter().any(|scope| scope == "chat:read") {
        eprintln!(
            "warning: the token has no chat:read scope ({}) - chat messages will not arrive",
            identity.scopes.join(", ")
        );
    }
    println!(
        "connected as {} for channel #{}",
        bot_login, config.channel
    );
    {
        let mut state = bot.state.lock().unwrap();
        state.bot_login = bot_login.to_ascii_lowercase();
        state.bot_user_id = identity.user_id.clone();
    }

    // Keep the list of the chatters of the channel up to date, it lets the
    // bot resolve names of people that have not written yet.
    {
        let bot = bot.clone();
        let config = config.clone();
        tokio::spawn(async move {
            loop {
                if let Err(err) = refresh_roster(&bot, &config).await {
                    eprintln!("roster: {err}");
                }
                tokio::time::sleep(std::time::Duration::from_secs(300)).await;
            }
        });
    }

    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<irc::IrcEvent>();
    let irc_client = irc::connect(
        config.irc.clone(),
        config.channel.clone(),
        bot_login.clone(),
        token.access_token.clone(),
        events_tx,
    )
    .await;

    println!("listening for chat messages, press Ctrl+C to stop");

    let mut ctrl_c = Box::pin(tokio::signal::ctrl_c());
    // The render tick makes sure the last message of a burst reaches the
    // overlay even when no further chat message arrives.
    let mut render_tick = tokio::time::interval(std::time::Duration::from_millis(500));
    render_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            event = events_rx.recv() => {
                let Some(event) = event else { break };
                match event {
                    irc::IrcEvent::Chat(message) => {
                        bot.on_chat(&message, &irc_client);
                    }
                    irc::IrcEvent::Notice { text } => {
                        eprintln!("twitch notice: {text}");
                    }
                    irc::IrcEvent::System(text) => eprintln!("{text}"),
                    irc::IrcEvent::Disconnected => eprintln!("irc: disconnected, retrying"),
                    irc::IrcEvent::AuthFailed => {
                        eprintln!("irc: authentication failed, getting a new token");
                        let refresh =
                            refresh_or_login(&client, &config, &mut auth_rx, &login_state);
                        tokio::pin!(refresh);
                        let refreshed = tokio::select! {
                            result = &mut refresh => result,
                            _ = wait_for_window_close(&mut window_closed) => {
                                println!("\ncomic window closed");
                                return Ok(());
                            }
                        };
                        match refreshed {
                            Ok(token) => {
                                let validated =
                                    auth::validate(&client, &token.access_token).await;
                                let allowed = match &validated {
                                    Ok(identity) => match configured_bot_login(&config, identity) {
                                        Ok(bot_login) => {
                                            let mut state = bot.state.lock().unwrap();
                                            state.bot_login = bot_login.to_ascii_lowercase();
                                            state.bot_user_id = identity.user_id.clone();
                                            true
                                        }
                                        Err(err) => {
                                            eprintln!("irc: {err}");
                                            false
                                        }
                                    },
                                    Err(err) => {
                                        eprintln!(
                                            "irc: could not validate the new token: {err}"
                                        );
                                        true
                                    }
                                };
                                if allowed {
                                    irc_client.update_token(token.access_token);
                                    eprintln!("irc: new token ready, reconnecting");
                                }
                            }
                            Err(err) => eprintln!("irc: could not get a new token: {err}"),
                        }
                    }
                }
            }
            _ = render_tick.tick() => {
                bot.tick();
            }
            _ = wait_for_window_close(&mut window_closed) => {
                println!("\ncomic window closed");
                break;
            }
            _ = &mut ctrl_c => {
                println!("\nbye");
                break;
            }
        }
    }
    Ok(())
}

/// Update the list of the chatters of the channel.
async fn refresh_roster(bot: &std::sync::Arc<Bot>, config: &Config) -> Result<()> {
    let client = https::HttpsClient::new()?;
    let Some(token) = user_token(&client, config).await? else {
        return Ok(());
    };
    let identity = auth::validate(&client, &token.access_token).await?;
    if !identity
        .scopes
        .iter()
        .any(|scope| scope == "moderator:read:chatters")
    {
        return Ok(());
    }
    let app = auth::app_token(&client, config).await?;
    let owner = auth::fetch_channel_owner(&client, config, &app, &config.channel).await?;
    let chatters = auth::fetch_chatters(
        &client,
        config,
        &token.access_token,
        &owner.broadcaster_id,
        &identity.user_id,
    )
    .await?;

    let mut state = bot.state.lock().unwrap();
    for chatter in chatters {
        let entry = bot::Identity {
            user_id: chatter.user_id.clone(),
            login: chatter.login.clone(),
            display_name: chatter.display_name.clone(),
        };
        state
            .by_login
            .insert(entry.login.to_lowercase(), entry.user_id.clone());
        state
            .by_display_name
            .insert(entry.display_name.to_lowercase(), entry.user_id.clone());
        state.roster.insert(entry.user_id.clone(), entry);
    }
    Ok(())
}

/// `comicbot login`
async fn login(config_path: &Path, args: &[String]) -> Result<()> {
    let config = prepare(config_path)?;
    let client = https::HttpsClient::new()?;
    let fonts = fonts::Fonts::load(&config.fonts)?;
    let bot = std::sync::Arc::new(Bot::new(config.clone(), fonts));
    let (auth_tx, mut auth_rx) = mpsc::unbounded_channel::<AuthEvent>();
    let login_state = start_server(bot, &config, auth_tx).await?;

    let scopes = if args.iter().any(|arg| arg == "--moderator") {
        auth::MODERATOR_SCOPES
    } else {
        auth::BOT_SCOPES
    };
    let token = login_interactively(&client, &config, &mut auth_rx, &login_state, scopes).await?;
    let identity = auth::validate(&client, &token.access_token).await?;
    println!("account: {} (scopes: {})", identity.login, identity.scopes.join(", "));
    if let Some(name) = config
        .bot_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        let name = name.trim_start_matches('@');
        if !name.eq_ignore_ascii_case(&identity.login) {
            eprintln!(
                "warning: logged in as {} but bot_name is '{name}'. The bot will refuse to \
                 start until you log in with the bot account or change bot_name.",
                identity.login
            );
        }
    }
    Ok(())
}

/// `comicbot whoami`
async fn whoami(config_path: &Path) -> Result<()> {
    let config = prepare(config_path)?;
    let client = https::HttpsClient::new()?;
    let Some(token) = user_token(&client, &config).await? else {
        println!("no token stored, run 'comicbot login' first");
        return Ok(());
    };
    let identity = auth::validate(&client, &token.access_token).await?;
    println!("login:    {}", identity.login);
    println!("user id:  {}", identity.user_id);
    println!("app id:   {}", identity.client_id);
    println!("scopes:   {}", identity.scopes.join(", "));
    match auth::app_token(&client, &config).await {
        Ok(app) => match auth::fetch_profile(&client, &config, &token.access_token, &app).await {
            Ok(profile) => {
                println!("display:  {}", profile.display_name);
                println!("login:    {}", profile.login);
                if let Some(image) = profile.profile_image {
                    println!("picture:  {image}");
                }
            }
            Err(err) => eprintln!("profile lookup failed: {err}"),
        },
        Err(err) => eprintln!("app token failed: {err}"),
    }
    Ok(())
}

/// `comicbot purge` - delete the messages the bot sent from the chat.
///
/// Twitch only allows this with a token that has the
/// `moderator:manage:chat_messages` scope.
async fn purge(config_path: &Path, args: &[String]) -> Result<()> {
    let config = prepare(config_path)?;
    let client = https::HttpsClient::new()?;
    let token = match user_token(&client, &config).await? {
        Some(token) => token,
        None => anyhow::bail!("no token stored, run 'comicbot login' first"),
    };
    let identity = auth::validate(&client, &token.access_token).await?;
    if !identity
        .scopes
        .iter()
        .any(|scope| scope == "moderator:manage:chat_messages")
    {
        anyhow::bail!(
            "the stored token cannot delete messages.\nRun 'comicbot login --moderator' once to get the extra permissions."
        );
    }

    let app = auth::app_token(&client, &config).await?;
    let owner = auth::fetch_channel_owner(&client, &config, &app, &config.channel).await?;
    let moderator_id = if identity.user_id == owner.broadcaster_id {
        owner.broadcaster_id.clone()
    } else {
        identity.user_id.clone()
    };

    let limit = args
        .first()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(100);
    println!(
        "channel #{} ({}), deleting messages of {}",
        owner.broadcaster_login, owner.broadcaster_name, identity.login
    );
    let messages = auth::fetch_chat_messages(
        &client,
        &config,
        &token.access_token,
        &owner.broadcaster_id,
        &moderator_id,
        limit,
    )
    .await?;

    let mut deleted = 0;
    for message in messages {
        let mine = message.user_id == identity.user_id
            || message.login.eq_ignore_ascii_case(&identity.login);
        if !mine {
            continue;
        }
        match auth::delete_chat_message(
            &client,
            &config,
            &token.access_token,
            &owner.broadcaster_id,
            &moderator_id,
            &message.message_id,
        )
        .await
        {
            Ok(()) => {
                deleted += 1;
                println!("deleted: {}", message.text);
            }
            Err(err) => eprintln!("could not delete '{}': {err}", message.text),
        }
    }
    println!("deleted {deleted} message(s)");
    Ok(())
}

/// `comicbot say <text>` - send a chat message with the stored token.
async fn say(config_path: &Path, args: &[String]) -> Result<()> {
    let text = args.join(" ");
    if text.trim().is_empty() {
        anyhow::bail!("usage: comicbot say <message>");
    }
    let config = prepare(config_path)?;
    let client = https::HttpsClient::new()?;
    let token = match user_token(&client, &config).await? {
        Some(token) => token.access_token,
        None => anyhow::bail!("no token available; run 'comicbot login' first"),
    };
    let identity = auth::validate(&client, &token).await?;
    let bot_login = configured_bot_login(&config, &identity)?;
    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<irc::IrcEvent>();
    let irc_client = irc::connect(
        config.irc.clone(),
        config.channel.clone(),
        bot_login.clone(),
        token,
        events_tx,
    )
    .await;

    // Give the connection a moment, then send.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    irc_client.send(&text);
    println!("sent as {}: {}", bot_login, text);

    // Wait for the echo so the message really left the client.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(irc::IrcEvent::Chat(message))) =
            tokio::time::timeout_at(deadline, events_rx.recv()).await
        {
            if message.text == text {
                println!("delivered");
                break;
            }
        }
    }
    Ok(())
}

/// `comicbot irc-check` - connect and report what Twitch answers.
async fn irc_check(config_path: &Path) -> Result<()> {
    let config = prepare(config_path)?;
    let client = https::HttpsClient::new()?;
    let token = match user_token(&client, &config).await? {
        Some(token) => token.access_token,
        None => anyhow::bail!("no token available; run 'comicbot login' or set COMICBOT_TOKEN"),
    };

    let identity = auth::validate(&client, &token).await?;
    let bot_login = configured_bot_login(&config, &identity)?;
    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<irc::IrcEvent>();
    let _client = irc::connect(
        config.irc.clone(),
        config.channel.clone(),
        bot_login,
        token,
        events_tx,
    )
    .await;

    println!("connecting to {}:{}", config.irc.server, config.irc.port);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, events_rx.recv()).await {
            Ok(Some(event)) => match event {
                irc::IrcEvent::Notice { text } => println!("notice: {text}"),
                irc::IrcEvent::System(text) => println!("system: {text}"),
                irc::IrcEvent::Chat(message) => {
                    println!("chat: {}: {}", message.display_name, message.text)
                }
                irc::IrcEvent::Disconnected => println!("disconnected"),
                irc::IrcEvent::AuthFailed => println!("authentication failed"),
            },
            Ok(None) => break,
            Err(_) => break,
        }
    }
    Ok(())
}

/// `comicbot logout`
fn logout(config_path: &Path) -> Result<()> {
    let config = prepare(config_path)?;
    auth::clear_token(&config.token_path());
    println!("token removed from {}", config.token_path().display());
    Ok(())
}

/// `comicbot app-token`
async fn app_token(config_path: &Path) -> Result<()> {
    let config = prepare(config_path)?;
    let client = https::HttpsClient::new()?;
    let token = auth::app_token(&client, &config).await?;
    println!("{token}");
    Ok(())
}

/// A transcript that can be rendered without connecting to Twitch.
#[derive(Debug, serde::Deserialize)]
struct Transcript {
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    subtitle: Option<String>,
    messages: Vec<TranscriptMessage>,
}

#[derive(Debug, serde::Deserialize)]
struct TranscriptMessage {
    login: String,
    #[serde(default)]
    display_name: Option<String>,
    text: String,
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    avatar: Option<PathBuf>,
}

/// `comicbot render <file.json>`
fn render_file(config_path: &Path, args: &[String]) -> Result<()> {
    let Some(input) = args.first() else {
        anyhow::bail!("usage: comicbot render <transcript.json> [--out <file.png>]");
    };
    let mut out = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join("comic.png"))
        .unwrap_or_else(|| PathBuf::from("comic.png"));
    let mut index = 1;
    while index < args.len() {
        if args[index] == "--out" {
            index += 1;
            out = PathBuf::from(args.get(index).context("--out needs a path")?);
        }
        index += 1;
    }

    let config = prepare(config_path)?;
    let fonts = fonts::Fonts::load(&config.fonts)?;
    let text = std::fs::read_to_string(input)?;
    let transcript: Transcript = serde_json::from_str(&text)?;

    let base = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let messages: Vec<ComicMessage> = transcript
        .messages
        .iter()
        .map(|message| ComicMessage {
            user_id: message.login.clone(),
            display_name: message
                .display_name
                .clone()
                .unwrap_or_else(|| message.login.clone()),
            text: message.text.clone(),
            color: message
                .color
                .as_deref()
                .and_then(Color::parse_hex)
                .unwrap_or(Color::rgb(90, 90, 90)),
            avatar: message.avatar.as_ref().map(|path| {
                if path.is_absolute() {
                    path.clone()
                } else {
                    base.join(path)
                }
            }),
            badges: Vec::new(),
            timestamp_ms: 0,
            reply: false,
        })
        .collect();

    let title = transcript
        .title
        .unwrap_or_else(|| format!("#{}", transcript.channel.clone().unwrap_or(config.channel.clone())));
    let subtitle = transcript.subtitle.unwrap_or_else(|| "comic chat".to_string());

    let characters: Vec<Character> = messages
        .iter()
        .map(|message| Character {
            display_name: message.display_name.clone(),
            color: message.color,
            avatar: message.avatar.clone(),
            messages: 1,
        })
        .collect();
    let cells: Vec<Cell> = messages
        .into_iter()
        .map(|message| Cell {
            first_time_ms: message.timestamp_ms,
            messages: vec![message],
        })
        .collect();
    let page = ComicPage {
        cells,
        characters,
        background: config.background.clone(),
    };
    let image = comic::render(&page, &fonts, &config.comic, &title, &subtitle);
    image.save(&out)?;
    println!("wrote {}", out.display());
    Ok(())
}

/// `comicbot demo` - render a comic without any network access.
fn demo(config_path: &Path) -> Result<()> {
    let config = prepare(config_path)?;
    let fonts = fonts::Fonts::load(&config.fonts)?;

    let demo_dir = config.data_dir.join("demo");
    std::fs::create_dir_all(&demo_dir)?;

    let people = [
        ("salmon85", "Salmon85", Color::rgb(0, 173, 255)),
        ("pixelpete", "PixelPete", Color::rgb(214, 66, 87)),
        ("mod_mira", "Mira", Color::rgb(122, 189, 0)),
    ];
    let lines = [
        ("salmon85", "guys the new build finally fixed the comic bubbles"),
        ("pixelpete", "does it still squash my avatar into a circle?"),
        ("mod_mira", "no, it keeps the aspect ratio now"),
        ("salmon85", "shipping it"),
        ("pixelpete", "can you give me the picture with the hat?"),
    ];

    let mut messages = Vec::new();
    for (login, name, color) in people {
        let path = demo_dir.join(format!("{login}.png"));
        if !path.exists() {
            placeholder_avatar(&path, name, color, &fonts.bold)?;
        }
    }
    let timestamps = [0i64, 300, 900, 1200, 1500];
    for ((login, text), timestamp_ms) in lines.into_iter().zip(timestamps) {
        let (login, name, color) = people
            .iter()
            .find(|(l, _, _)| *l == login)
            .copied()
            .unwrap_or(("salmon85", "Salmon85", Color::rgb(90, 90, 90)));
        messages.push(ComicMessage {
            user_id: login.to_string(),
            display_name: name.to_string(),
            text: text.to_string(),
            color,
            avatar: Some(demo_dir.join(format!("{login}.png"))),
            badges: Vec::new(),
            timestamp_ms,
            reply: false,
        });
    }

    let mut characters: Vec<Character> = Vec::new();
    for message in &messages {
        match characters
            .iter_mut()
            .find(|character| character.display_name == message.display_name)
        {
            Some(character) => character.messages += 1,
            None => characters.push(Character {
                display_name: message.display_name.clone(),
                color: message.color,
                avatar: message.avatar.clone(),
                messages: 1,
            }),
        }
    }
    let window_ms = config.comic.cell_window_ms();
    let mut cells: Vec<Cell> = Vec::new();
    for message in messages {
        let starts_new_cell = cells.last().map_or(true, |cell| {
            message.timestamp_ms.saturating_sub(cell.first_time_ms) >= window_ms
        });
        if starts_new_cell {
            cells.push(Cell {
                first_time_ms: message.timestamp_ms,
                messages: vec![message],
            });
        } else if let Some(cell) = cells.last_mut() {
            cell.messages.push(message);
        }
    }
    let page = ComicPage {
        cells,
        characters,
        background: config.background.clone(),
    };
    let image = comic::render(
        &page,
        &fonts,
        &config.comic,
        &format!("#{}", config.channel),
        "demo comic",
    );
    let out = demo_dir.join("demo.png");
    image.save(&out)?;
    println!("wrote {}", out.display());
    Ok(())
}

/// Draw a simple character so the demo works out of the box.
///
/// The picture has a transparent background and is taller than wide, the same
/// way a real character drawing would be.
fn placeholder_avatar(
    path: &Path,
    name: &str,
    color: Color,
    font: &render::Font,
) -> Result<()> {
    let width = 220usize;
    let height = 300usize;
    let mut canvas = render::Canvas::new(width, height, Color::TRANSPARENT);

    // Body.
    canvas.fill_round_rect(40, 120, 140, 180, 60, color);
    // Head.
    canvas.fill_round_rect(68, 24, 84, 104, 42, Color::rgb(245, 214, 180));
    // Hair.
    canvas.fill_round_rect(62, 12, 96, 52, 26, Color::rgb(70, 50, 40));

    let initial = name
        .chars()
        .next()
        .unwrap_or('?')
        .to_uppercase()
        .to_string();
    let size = 70.0f32;
    let text_width = font.measure(size, &initial);
    font.draw(
        &mut canvas,
        size,
        (width as f32 - text_width) / 2.0,
        230.0,
        &initial,
        Color::WHITE,
    );
    canvas.into_image().save(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(login: &str) -> auth::Identity {
        auth::Identity {
            client_id: String::new(),
            login: login.to_string(),
            user_id: "1".to_string(),
            scopes: Vec::new(),
        }
    }

    #[test]
    fn bot_name_must_match_the_token_account() {
        let identity = identity("mybot");
        let mut config = Config::default();
        assert_eq!(configured_bot_login(&config, &identity).unwrap(), "mybot");

        config.bot_name = Some("@MyBot".to_string());
        assert_eq!(configured_bot_login(&config, &identity).unwrap(), "mybot");

        config.bot_name = Some("someoneelse".to_string());
        assert!(configured_bot_login(&config, &identity).is_err());
    }
}
