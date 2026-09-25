# pnl-bot

A Telegram bot that shows your realized PnL from MoonBot cores, for **today** and
**this month**. It connects to each core over MoonProto and keeps a local copy of
the core's `Orders` report, so answers come instantly and survive restarts.

Each user runs their own copy on their own VPS, with their own bot from @BotFather.
The bot answers only its owner.

- Add and delete any number of cores from the chat with their MoonProto key.
- Each core's trades are shown separately, then totalled. Emulator trades are listed apart from real ones.

## Using it

`/start` opens the menu:

- **📊 Today** / **📅 Month** — PnL per core and in total, sent as a table image (or as text, see Settings):
  orders, wins/losses, volume, average order, profit, profit % of volume.
  **🔄 Refresh** updates the message.
- **🖥 Cores** — the cores and their state (🟢 live, 🟡 connecting or syncing, 🟠 reconnecting, 🔴 offline).
  The list shows 7 cores per page. With more, **◀ Prev** / **Next ▶** turn the pages.
  - **➕ Add core** — send a name, then the key string MoonBot exports for MoonProto.
    The bot deletes your message with the key right after reading it. If the key has no address
    (older exports), the bot also asks for `host:port`.
  - **📋 Add several** — send one message listing several cores, one per line: a name, then the key.
    The bot deletes the message right after reading it, adds every valid line, and lists the lines it
    skipped, by line number and reason:

    ```
    Binance futures  KEY
    Gate: KEY
    KEY
    Old core  KEY  203.0.113.5:4545
    ```

    A line with only a key is named after MoonBot's own label for the key. A key without an address
    needs `host:port` after it. Empty lines and lines starting with `#` are skipped.
  - **🗑 name** — disconnects from that core and deletes the bot's local copy of its history.
    Nothing changes on the core.
- **⚙️ Settings**
  - **Emulator trades:** shown or hidden in reports. Shown by default.
  - **Reports come as:** a table image (default) or a text message. The text report is a monospace
    table narrow enough for a phone — orders, wins/losses, profit and profit % — so it leaves out
    volume, average order and the exchange.

  Both are saved with the bot's data and survive restarts.

Commands: `/menu`, `/today`, `/month`, `/cores`, `/settings`, `/cancel`.

Figures are closed trades, counted by close time. Open positions are not included.
Reports use the core's clock. If your cores do not run on UTC, set `REPORT_UTC_OFFSET_MINUTES`.

## Install on a VPS

1. In Telegram, open [@BotFather](https://t.me/BotFather), send `/newbot`, and copy the token it gives you.
2. From the [latest release](https://github.com/Asmodeios/moonproto-pnl-bot/releases/latest), download the
   archive for the VPS's architecture (`uname -m`): `pnl-bot-linux-x86_64.tar.gz` or `pnl-bot-linux-aarch64.tar.gz`.
   Copy it to the VPS (`scp pnl-bot-linux-x86_64.tar.gz user@VPS_IP:~`), or download it there with
   `gh release download --repo Asmodeios/moonproto-pnl-bot --pattern 'pnl-bot-linux-x86_64.tar.gz'`.
3. On the VPS (Ubuntu 22.04 or 24.04), unpack it and run the installer:

   ```sh
   tar -xzf pnl-bot-linux-x86_64.tar.gz && cd pnl-bot
   sudo ./install.sh
   ```

   The installer asks for the token and starts the service. The binary is prebuilt and static,
   so nothing is compiled on the VPS.
4. The installer prints a link like `https://t.me/your_bot?start=…`. Open it in Telegram and
   press **Start**. Whoever opens it first becomes the owner, for good. Nobody else can use the bot after that.
5. In the bot, go to **🖥 Cores → ➕ Add core** and add your cores.

To update, unpack a newer release the same way and run `sudo ./install.sh` again. It keeps the token, the owner and the cores.

If you know your numeric Telegram user id, you can set `OWNER_ID` in `/etc/pnl-bot.env` instead of
using the link. A restart while the bot has no owner prints a new link:
`journalctl -u pnl-bot | grep t.me`.

## Requirements

- Ubuntu 22.04 or 24.04 LTS (any recent x86_64 or arm64 Linux works).
- The bot uses a few tens of MB of RAM. Building from source needs about 2 GB.
- The cores' MoonProto UDP port must be reachable from the server. The bot opens no ports of its own.
- A bot token from [@BotFather](https://t.me/BotFather).

## Manual build and install

Run from a source checkout instead of a release archive, `install.sh` builds the bot itself. To do it by hand, build on the server or on any Linux machine with the same architecture:

```sh
sudo apt update && sudo apt install -y build-essential pkg-config git curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
cd moonproto-pnl-bot
cargo build --release
```

SQLite and TLS are compiled in, so the binary needs no extra system libraries.

Then install the service:

```sh
sudo install -D -m 0755 target/release/pnl-bot /opt/pnl-bot/pnl-bot
sudo install -m 0600 .env.example /etc/pnl-bot.env
sudo nano /etc/pnl-bot.env            # set BOT_TOKEN
sudo install -m 0644 deploy/pnl-bot.service /etc/systemd/system/pnl-bot.service
sudo systemctl daemon-reload
sudo systemctl enable --now pnl-bot
journalctl -u pnl-bot -f              # logs
```

The unit runs the bot as a throwaway system user. Its data lives in `/var/lib/pnl-bot`: `cores.json`
(the owner, and the cores with their keys) and `reports/*.sqlite3`. Only root and that user can read it.
Leave `DATA_DIR` out of the env file, because a value there overrides the unit's.

## Releasing

Bump `version` in `Cargo.toml`, commit, then tag it and push the tag: `git tag v0.1.1 && git push origin v0.1.1`.
The [Release workflow](.github/workflows/release.yml) builds static x86_64 and aarch64 binaries, packs each with
the installer, and publishes them with `SHA256SUMS` as a GitHub Release. It fails if the tag doesn't match `Cargo.toml`.
Run it from the Actions tab to build the archives without releasing.

## Configuration

| Variable | Default | |
|---|---|---|
| `BOT_TOKEN` | — | Required. |
| `OWNER_ID` | empty | Your Telegram user id, to skip the claim link. Once set, it replaces any owner stored earlier. |
| `DATA_DIR` | `data` | Where `cores.json` and the report replicas are kept. |
| `REPORT_UTC_OFFSET_MINUTES` | `0` | The cores' clock against UTC, e.g. `180` for UTC+3. |
| `RUST_LOG` | `info,moonproto=warn` | Log filter. |

To run it by hand, put these in a `.env` next to where you start it, then run `cargo run --release`.

## Security notes

- A MoonProto key gives full control of its core. `cores.json` stores keys in plain text, protected only
  by file permissions (0600 inside a 0700 directory). There is no OS keystore to seal them with on
  a server. Keep the server and its backups private.
- Keys are never written to the logs. Telegram API errors are logged without the request URL,
  because that URL contains the bot token.
- The bot works only in private chats, so a key is never pasted where others can read it.
- Anyone can find a bot on Telegram, so ownership is set only by the one-time claim code
  (12 random characters, printed only to the server log) or by `OWNER_ID`. Wrong codes are logged.

## How it works

| File | Role |
|---|---|
| `src/main.rs` | Startup: config, store, token check (`getMe`), the claim code while the bot has no owner, one session per core, the bot loop, graceful shutdown on SIGTERM. |
| `src/config.rs` | Environment settings. |
| `src/store.rs` | `cores.json`: the owner and the cores. Atomic writes, owner-only permissions. |
| `src/cores.rs` | MoonProto sessions: link state, the replica per session, and a reconnect every minute for cores whose connect failed. |
| `src/replica.rs` | The report replica: schema migration, paged catch-up, live upserts and deletes, alive-map reconciliation, and a checkpoint stored with it. |
| `src/pnl.rs` | Period bounds and the SQL tally over a replica. |
| `src/table.rs` | The report drawn as a PNG table (SVG rasterized by resvg; JetBrains Mono, OFL, built in from `assets/fonts/`). |
| `src/telegram.rs` | The Bot API calls (long polling; photo uploads for the report). |
| `src/bot.rs` | Claiming, screens, buttons, and the add/delete flow. |

The MoonProto crate is pinned to a commit in `Cargo.toml`.
