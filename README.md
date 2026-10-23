# comicbot

A Twitch chat bot that turns a chat into a comic page, in the spirit of the
old **Microsoft Comic Chat**: every chatter gets a picture of their own and
every message becomes a speech bubble.

The pictures are **your own PNG files** - drop them into `avatars/` and assign
them to people with a chat command or in the config file.

Messages arrive in cells: everything said within one second shares a panel, so
several chatters can appear side by side in the same cell. The newest cell is
always drawn first (top left); older cells follow it. Each cell has a caption
box, the characters standing in the panel and the messages spoken in bubbles
that overlap them:

```
+---------+ +---------+ +---------+
| SALMON85| | PIXELPET| | MIRA    |
|  ____   | |  ____   | |  ____   |
| /      \| | /      \| | /      \|   characters with the speech
| |  (o) | | |  (o) | | |  (o) ||   bubble over their head
| |      | | |      | | |      ||
| | hello| | | hi   | | | ok   ||
| |      | | |      | | |      ||
+---------+ +---------+ +---------+
```

## Features

* Real comic pages: a grid of panels with caption boxes, characters standing
  in the panels and speech bubbles that overlap them
* Chat is grouped into one-second cells, so several chatters can share a panel
* The newest cell is always displayed first; older cells roll off the page
* A configurable page size (`columns` × `rows`)
* A roster of all characters on the right side of the page, like the sidebar
  of the original Comic Chat
* Your own PNG files as the characters, assigned per chatter, assigned from chat
* New chatters automatically get an unused random avatar; reserved avatars can
  be kept for moderators
* Your own PNG backgrounds behind every cell, switchable from chat
* Twitch chat colours and moderator/subscriber/VIP badges
* Automatic rendering at a short interval, or on demand with `!comic`
* A native comic window that updates as soon as a new page is rendered, ready
  for an OBS window capture
* PNG files are still written to `data/comics/` for the archive

## Build

Linux/macOS:

```sh
cargo build --release
./target/release/comicbot --help
```

Windows (native):

```powershell
cargo build --release
.\target\release\comicbot.exe --help
```

Cross-compile a Windows exe from Linux (requires `mingw-w64`):

```sh
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
wine target/x86_64-pc-windows-gnu/release/comicbot.exe --help
```

The comic window uses Win32 on Windows and X11 on Linux, so the same source
builds for both.

## Setup

The configuration lives in `comicbot.toml`; it is created automatically on the
first run with the values below. Replace the client id and secret with the
credentials of your own Twitch application.

```toml
channel = "salmon85"
bot_name = ""             # optional: Twitch login of the bot account
client_id = "YOUR_CLIENT_ID"
client_secret = "YOUR_CLIENT_SECRET"
redirect_uri = "http://localhost:3000"
```

### 1. Log in

Twitch IRC needs a **user token** with the `chat:read` scope. Log in with the
account that should be the bot (usually a separate bot account):

```sh
cargo run --release -- login
```

The program starts the local web server on `http://localhost:3000` and opens
an authorization URL in your default browser. Approve the request, and the
token is stored in `data/token.json` (mode `0600`). On a headless system the
browser cannot be opened; the URL is printed so you can open it elsewhere.

You can also open <http://localhost:3000/auth> in the browser instead of
copying the URL.

> The redirect URI `http://localhost:3000` must be registered in the Twitch
> developer console for the application, together with the `chat:read` and
> `chat:edit` scopes.

### 2. Run the bot

```sh
cargo run --release -- run
```

The bot joins `#salmon85`, renders a comic strip for new messages and shows it
in the comic window.

## Giving people comic characters

1. Put your PNG files into the `avatars/` directory. Any size works, the aspect
   ratio is kept and the picture is scaled into the panel.
2. Assign a picture to a chatter:

   ```
   !setavatar pixelpete pirate       # uses avatars/pirate.png
   !setavatar pixelpete pirate.png   # the same, with the file name
   ```

   Assignments are written to `comicbot.toml` and survive a restart.
3. Remove an assignment again with `!clearavatar pixelpete`.

The command takes the name as it is shown in the chat, the login and a part of
the display name also work. The picture itself is stored under the stable user
id of the chatter, so somebody who changes their name keeps their character.

Assignments can also be edited by hand. The key is the user id, the name is
only there to make it readable:

```toml
[avatars]
123456789 = "avatars/pirate.png"
```

A picture that is used for everybody can be set with `default_avatar`.

### Automatic avatars

A chatter who has no assigned picture gets a random avatar from `avatars/` the
first time they write a normal message (unless `default_avatar` is set). The
assignment is written to `comicbot.toml`, so the character stays with them
across restarts. Random picks prefer pictures that are not already in use by
somebody else.

Pictures that should stay available for moderators can be listed with
`mod_avatars` in the top level of the config. They are never picked
automatically, but they are still listed by `!avatars` and can be assigned
manually:

```toml
mod_avatars = ["mod_hat", "mod_crown"]
```

## Cell backgrounds

1. Put your PNG files into the `backgrounds/` directory.
2. Switch the background of every cell:

   ```
   !setbackground paper       # uses backgrounds/paper.png
   !setbackground paper.png   # the same, with the file name
   ```

   The selection is written to `comicbot.toml` and survives a restart.
3. List the available backgrounds with `!backgrounds` and show the current one
   with `!background`.

Only moderators, the broadcaster and the channel owner can change the
background. Backgrounds are scaled to cover each cell and cropped from the
centre; the characters and speech bubbles are drawn on top.

## Running the bot on your own account

The bot can use the same account you chat with - every message you write then
shows up in the comic, and the bot only ignores the answers it sent itself.
A separate bot account is still nicer, because then the comic link and the
answers of the bot do not clutter your own chat.

To use a separate bot name, log in with a separate Twitch account and set
`bot_name` to that account's login. Twitch ties the IRC nickname to the OAuth
token, so `bot_name` cannot rename an account: it makes comicbot verify that
the intended bot account is logged in and stop with an explanation if it is
not.

Every message the bot receives is printed to the terminal:

```
chat: Salmon85: hello from the chat
```

If Twitch rejects the login token, the bot refreshes the stored token
automatically and reconnects. When no refresh token is available it starts the
interactive login again. A token supplied through `COMICBOT_TOKEN` cannot be
refreshed, so replace it and restart the bot if it gets rejected.

## How the bot behaves in chat

The bot is quiet in the chat. It only answers the commands below and every
answer starts with `BOT:`, so it is always clear that a message was written by
the bot. The answers of the bot are never part of the comic.

```
chat: Salmon85: hello everyone
sent: BOT: available pictures: salmon, pirate
```

## Chat commands

| Command | What it does |
| --- | --- |
| `!comic` | render the strip now and post the link |
| `!avatar` | show which picture you are using |
| `!avatars` | list the available pictures |
| `!setavatar <name> <picture>` | assign a picture to a chatter (moderators) |
| `!clearavatar <name>` | remove the picture of a chatter (moderators) |
| `!background` | show the current cell background |
| `!backgrounds` | list the available backgrounds |
| `!setbackground <picture>` | switch the cell background (moderators) |
| `!whois <name>` | show the picture of a chatter |
| `!help` | show the list of commands |

## Removing messages from the chat

The bot can delete its own messages from the chat history. Twitch needs a
token with the `moderator:manage:chat_messages` scope for that, so log in
once with the extra permissions:

```sh
comicbot login --moderator
comicbot purge          # deletes the messages of the bot account
```

## The comic window

`comicbot run` opens a native window titled `comicbot` that shows the newest
page. There is no browser view and no polling: the renderer sends each finished
page straight to the window thread, which replaces the window contents
immediately.

Closing the comic window also stops comicbot. For OBS, add a **Window Capture**
source and select the `comicbot` window. The window opens at the configured
comic width and scales the page to fit, so you can resize it without the comic
becoming blurry or cropped. On Linux the
window is an X11 window (XWayland works too), which is the window type OBS
window capture handles best. If the bot runs without a display server (for
example on a headless server), the window cannot be opened; the bot logs a
warning and keeps writing PNG files.

The local HTTP server is still used for the Twitch OAuth callback and for
`http://localhost:3000/api/status`, which returns JSON with the channel,
participants and counters. Generated pages are also stored in `data/comics/`
and the newest one is copied to `data/comics/latest.png`.

## Configuration

```toml
render_mode = "each"      # "each" (automatic), "manual" (only !comic) or "off"
backgrounds_dir = "backgrounds"  # PNG pool for !setbackground
# background = "backgrounds/paper.png"  # selected with !setbackground
# mod_avatars = ["mod_hat", "mod_crown"]  # never picked at random

[comic]
width = 1200              # width of the generated page
columns = 3               # panels per row
rows = 2                  # panel rows (the page shows columns * rows cells)
sidebar_width = 210       # width of the character roster on the right
show_sidebar = true       # set to false for a page without the roster
font_size = 17.0          # text inside the speech bubbles
name_font_size = 16.0     # text of the caption boxes and names
cell_window_secs = 1.0    # messages within this window share one cell
max_chatters_per_cell = 4 # cap on distinct speakers in one cell
max_message_chars = 200   # truncate longer messages with ...
min_interval_secs = 1     # minimum delay between two automatic pages
transparent_background = false
show_header = true
```

A cell collects every message that arrives within `cell_window_secs` of the
first message of that cell, up to `max_chatters_per_cell` different speakers.
Messages from speakers already in the cell still join it; a new speaker starts
another cell. Message text longer than `max_message_chars` is cut off with
`...`. The page holds at most `columns * rows` cells;
when a newer cell is created, the oldest cell rolls off. The newest cell is
drawn in the top left corner. With `columns = 1`
the page becomes a single vertical strip, `columns = 2` a two column strip.
More columns on a wider page look more like a real comic book page.
[http]
host = "127.0.0.1"
port = 3000

[irc]
server = "irc.chat.twitch.tv"
port = 6697
send_delay_ms = 1200      # rate limit for outgoing chat messages
```

Environment variables override the file: `COMICBOT_CHANNEL`,
`COMICBOT_BOT_NAME`, `COMICBOT_CLIENT_ID`, `COMICBOT_CLIENT_SECRET`,
`COMICBOT_REDIRECT_URI`, `COMICBOT_DATA_DIR`, `COMICBOT_AVATARS_DIR`,
`COMICBOT_BACKGROUNDS_DIR` and `COMICBOT_TOKEN` (a ready made user token,
useful on a server without a browser).

## Other commands

```sh
comicbot demo                    # render a demo comic with generated pictures
comicbot render transcript.json  # render a strip from a JSON transcript
comicbot whoami                  # show which account the token belongs to
comicbot app-token               # print an app access token
comicbot irc-check               # test the connection to Twitch IRC
comicbot say "hello chat"        # send a message, handy for testing
comicbot purge                   # delete the bot's messages from the chat
comicbot login --moderator       # log in with the extra moderator permissions
comicbot logout                  # delete the stored token
```

A transcript file looks like this:

```json
{
  "channel": "salmon85",
  "title": "#salmon85",
  "subtitle": "a quiet evening",
  "messages": [
    {
      "login": "salmon85",
      "display_name": "Salmon85",
      "text": "hello!",
      "color": "#00ADFF",
      "avatar": "avatars/salmon.png"
    }
  ]
}
```

## How it works

| Module | Purpose |
| --- | --- |
| `src/irc.rs` | Twitch IRC over TLS: tags, `PRIVMSG`, `PING`, reconnect, rate limited sending |
| `src/auth.rs` | OAuth authorization code flow, app tokens, token storage |
| `src/https.rs` | small HTTPS client used for the Twitch APIs |
| `src/server.rs` | small HTTP server for the OAuth callback and status endpoint |
| `src/display.rs` | native window that shows the newest comic |
| `src/render.rs` | software rasterizer: shapes, images and anti aliased text |
| `src/comic.rs` | comic page layout: panels, caption boxes, speech bubbles, roster |
| `src/avatars.rs` | picture pool and assignments |
| `src/bot.rs` | chat state, commands and rendering |

## Notes on security

* `client_secret` and the OAuth token live in `comicbot.toml` and
  `data/token.json`; the token file is written with mode `0600`. Add both to
  `.gitignore` and do not share them.
* The local web server binds to the loopback interface only.
* The bot can only change pictures of the pool that is inside `avatars/`, and
  file names that are served are validated against directory traversal.
