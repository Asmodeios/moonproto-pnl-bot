# pnl-bot

A Telegram bot that shows your profit and loss (PnL) from MoonBot cores.
It shows the PnL for **today** and for **this month**. Only closed trades are counted.

The bot connects to each core with MoonProto. It keeps a local copy of each core's `Orders` report.
Because of this, answers are fast, and the data is still there after a restart.

Each user runs their own copy of the bot on their own server (VPS), with their own bot from @BotFather.
The bot answers only one person: its owner.

- You can add and delete any number of cores in the chat. You need the core's MoonProto key.
- The bot shows each core on its own line, then the total. Emulator trades are shown apart from real trades.

## How to use the bot

Send `/start` to open the menu.

- **⏱ Hour** / **📊 Today** / **📅 Month** — PnL for each core and the total. **⏱ Hour** covers the
  last 60 minutes. The bot sends it as a table picture (or as text, see Settings). The table has:
  orders, wins/losses, volume, average order and profit.
  **🔄 Refresh** updates the message.
  **⏱ Hour** and **📊 Today** open **🖥 By core**, with a button to change to **🪙 By coin**.
  **📅 Month** opens **📆 By date**, with buttons to change to the other views:
  - **🖥 By core** — the same table, one row for each core.
  - **🪙 By coin** — all cores added together, one row for each coin. The coins with the biggest profit
    or loss are first. Emulator trades of a coin are shown in a separate row under it. If there are more
    than 20 coins, the table shows 20 of them, and one row ("N others") with the sum of the rest.
  - **📆 By date** — all cores added together, one row for each day with trades. The newest day is first.
    Each row has: orders, wins/losses, volume and profit. The last row is the total for the month.
    Emulator trades are shown only in a separate total row.

  **🔤 Sort by name** / **💰 Sort by profit** on **🖥 By core** and **🪙 By coin** switches the row order
  between profit (the default) and name. The bot remembers the choice. With more than 20 coins, the same
  20 are shown either way.

  Every report also has buttons to open the other periods.
- **🖥 Cores** — your cores and their state:
  🟢 live, 🟡 connecting or syncing, 🟠 reconnecting, 🔴 offline.
  One page shows 7 cores. If you have more, use **◀ Prev** / **Next ▶**.
  - **➕ Add core** — send a name, then the MoonProto key from MoonBot.
    The bot deletes your message with the key right after it reads it.
    Old keys have no address. For these, the bot also asks for `host:port`.
  - **📋 Add several** — send one message with many cores, one core on each line: a name, then the key.
    The bot deletes the message right after it reads it. It adds every correct line. Then it tells you
    which lines it skipped (line number and reason). Example:

    ```
    Binance 1  KEY
    Binance 2: KEY
    KEY
    Binance 4  KEY  203.0.113.5:4545
    ```

    If a line has only a key, the bot uses the name that MoonBot saved in the key.
    If a key has no address, write `host:port` after it.
    The bot skips empty lines and lines that start with `#`.
  - **✏️ name** — gives the core a new name. Send the new name as a message.
  - **🗑 name** — disconnects from this core and deletes the bot's local copy of its history.
    Nothing changes on the core itself.
- **⚙️ Settings**
  - **Emulator trades:** show or hide them in reports. They are hidden by default.
  - **Reports come as:** a text message (default) or a table picture. The text report is a small table
    that fits on a phone screen. It has only orders, wins/losses and profit.
    It does not show volume, average order or the exchange.
  - **Live trades:** on or off (default off). When it is on, the bot tells you about each trade your cores
    close: the coin, long or short, the core, profit and profit %, how long the trade was open, the sell
    reason and the strategy (for a liquidation, the strategy from its signal type). The bot waits 5 seconds
    after a trade closes and sends all trades closed in that time, on all cores, in one message. So many
    trades at once do not make many messages. Emulator trades come only if they are shown in reports.
    Trades that closed more than 10 minutes ago (for example, while the core or the bot was offline) are
    not sent, but they are still counted in the reports.

  The bot saves all settings. They stay after a restart.
- **⬆️ Update to vX.Y.Z** — the bot checks GitHub for a new version when it starts and then every hour.
  When there is one, this button shows in the main menu. Press it and confirm: the server installs the
  new version and restarts the bot, and the bot tells you when it is back. If the update fails, the bot
  tells you that too. See [Update](#update).

Commands: `/menu`, `/hour`, `/today`, `/month`, `/cores`, `/settings`, `/cancel`.

The numbers count only closed trades, by the time they closed. Open positions are not counted.
Reports use the clock of the core. If your cores do not use UTC time, set `REPORT_UTC_OFFSET_MINUTES`.

## Install on a VPS

1. In Telegram, open [@BotFather](https://t.me/BotFather) and send `/newbot`. Copy the token it gives you.
2. On your computer, open the **Releases** page of this repository. Download the archive for your VPS.
   To know which one you need, run `uname -m` on the VPS:

   | `uname -m` | Archive |
   |---|---|
   | `x86_64` | `pnl-bot-linux-x86_64.tar.gz` |
   | `aarch64` | `pnl-bot-linux-aarch64.tar.gz` |

3. Copy the archive to the VPS, with `scp` or copied manually. For `scp`, run this in the folder where you downloaded it:

   ```sh
   scp pnl-bot-linux-x86_64.tar.gz root@VPS_IP:~
   ```

   Change `root` to your login and `VPS_IP` to the address of your server.
   On Windows, `scp` works in PowerShell. You can also use WinSCP.
4. On the VPS (Ubuntu 22.04 or 24.04), unpack the archive and run the installer:

   ```sh
   cd ~
   tar -xzf pnl-bot-linux-x86_64.tar.gz
   cd pnl-bot
   sudo ./install.sh
   ```

   The installer then:

   1. Asks for the bot token. Paste the token from @BotFather.
   2. Asks for a passphrase (see [Passphrase](#passphrase)). Press Enter if you do not want one.
   3. Starts the bot.

   The program is already built, so the VPS does not need to compile or download anything.
5. The installer shows a link like `https://t.me/your_bot?start=…`. Open it in Telegram and press **Start**.
   The first person who opens the link becomes the owner. This cannot be changed later from the chat.
   Nobody else can use the bot.
   If you set a passphrase, the bot now asks you for it.
6. In the bot, go to **🖥 Cores → ➕ Add core** and add your cores.

### Update

When a new version is out, the bot's main menu shows **⬆️ Update to vX.Y.Z** (it checks GitHub
every hour). Press it and confirm: the server installs the update the same way as `pnl --update`
and restarts the bot. The bot tells you when it is back. This button works on installs from version
0.1.8 on; before that, update once with `pnl --update`.

Or run on the VPS:

```sh
pnl --update
```

It downloads the newest release for your CPU from GitHub, checks it against `SHA256SUMS`,
installs it and restarts the bot. If you already have the newest version, it does nothing
(`pnl --update --force` installs it again). If you use a passphrase, send it to the bot again after the update.

To update by hand (or from a version older than 0.1.4, which has no `pnl --update`), copy the new archive
to the VPS in the same way as the first time. Then run on the VPS:

```sh
cd ~
rm -rf pnl-bot
tar -xzf pnl-bot-linux-x86_64.tar.gz
cd pnl-bot
sudo ./install.sh
```

The update keeps your token, owner and cores. They are saved in `/etc/pnl-bot.env` and `/var/lib/pnl-bot`,
not in the folder you unpacked.

If you know your Telegram user id (a number), you can put it in `OWNER_ID` in `/etc/pnl-bot.env`.
Then you do not need the link. If the bot has no owner, it makes a new link after every restart.
To see it, run `pnl --link`.

## Control the bot: the `pnl` command

The installer adds the `pnl` command. Use it on the VPS to control the bot.
It asks for your password (with `sudo`) when it needs it.

| Command | What it does |
|---|---|
| `pnl --start` | Start the bot. |
| `pnl --stop` | Stop the bot. |
| `pnl --restart` | Restart the bot. If you use a passphrase, the bot asks for it again in Telegram. |
| `pnl --status` | Show if the bot is running, and the last lines of the log. |
| `pnl --logs` | Show the log live. Press Ctrl+C to exit. |
| `pnl --logs 100` | Show the last 100 lines of the log and exit. |
| `pnl --link` | Show the link to become the owner (only while the bot has no owner). |
| `pnl --passphrase` | Set, change or remove the [passphrase](#passphrase). |
| `pnl --update` | Download and install the newest release, see [Update](#update). |
| `pnl --help` | Show this list. |

The bot runs as a system service named `pnl-bot`. If it crashes or the server reboots,
it starts again by itself. After `pnl --stop`, it stays stopped until `pnl --start` or the next reboot.

## Passphrase

A passphrase encrypts the core keys in `cores.json` (with Argon2id and XChaCha20-Poly1305).
The bot never saves the passphrase.

- After every start (also after a restart or an update), the bot is locked. It sends you a message in
  Telegram. It does nothing until you send the passphrase.
- The bot deletes your message with the passphrase right after it reads it.
- After 3 wrong tries in a row, you must wait one minute.
- To set, change or remove the passphrase, run `pnl --passphrase` on the VPS. It stops the bot,
  encrypts the keys again, and starts the bot again. You can also add a passphrase later this way.
- If you forget the passphrase, press **Forgot it?** under the lock message in the bot.
  **Reset** deletes the saved cores. Then add them again. They are not encrypted until you run
  `pnl --passphrase` again.

## Requirements

- Ubuntu 22.04 or 24.04 LTS. Other new x86_64 or arm64 Linux systems also work.
- The bot needs little RAM (tens of MB). To build it from source, you need about 2 GB.
- The server must be able to reach the MoonProto UDP port of each core. The bot does not open any ports.
- A bot token from [@BotFather](https://t.me/BotFather).

## Build and install by hand

If you run `install.sh` from the source code (not from a release archive), it builds the bot for you.

To do everything by hand, build on the server, or on another Linux computer with the same CPU type
(`uname -m`):

```sh
sudo apt update && sudo apt install -y build-essential pkg-config git curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
cd moonproto-pnl-bot
cargo build --release
```

SQLite and TLS are built into the program, so it does not need other system libraries.

Then install the service:

```sh
sudo install -D -m 0755 target/release/pnl-bot /opt/pnl-bot/pnl-bot
sudo install -m 0755 deploy/pnl-bot-passphrase.sh /usr/local/sbin/pnl-bot-passphrase
sudo install -m 0755 deploy/pnl.sh /usr/local/bin/pnl
sudo install -m 0600 .env.example /etc/pnl-bot.env
sudo nano /etc/pnl-bot.env            # set BOT_TOKEN
sudo install -m 0644 deploy/pnl-bot.service /etc/systemd/system/pnl-bot.service
sudo systemctl daemon-reload
sudo systemctl enable --now pnl-bot
pnl --logs                            # watch the log
```

The service runs the bot as a temporary system user that systemd creates. The data is in `/var/lib/pnl-bot`:
`cores.json` (the owner, and the cores with their keys) and `reports/*.sqlite3`.
Only root and this user can read it.
Do not put `DATA_DIR` in the env file, because it would replace the value from the service file.

## Release a new version

1. Change `version` in `Cargo.toml` and commit.
2. Create a tag and push it: `git tag v0.1.1 && git push origin v0.1.1`.

The [Release workflow](.github/workflows/release.yml) builds static programs for x86_64 and aarch64.
It packs each one with the installer and publishes them with `SHA256SUMS` as a GitHub Release.
It fails if the tag is not the same as the version in `Cargo.toml`.
You can also start it from the Actions tab. Then it builds the archives but does not publish a release.

## Settings (environment variables)

| Variable | Default | Meaning |
|---|---|---|
| `BOT_TOKEN` | — | The bot token. Required. |
| `OWNER_ID` | empty | Your Telegram user id, so you do not need the owner link. If set, it replaces the saved owner. |
| `DATA_DIR` | `data` | The folder for `cores.json` and the local report copies. |
| `REPORT_UTC_OFFSET_MINUTES` | `0` | The difference between the cores' clock and UTC, in minutes. For example `180` for UTC+3. |
| `RUST_LOG` | `info,moonproto=warn` | How much to write to the log. |

To run the bot by hand, put these in a `.env` file in the folder where you start it.
Then run `cargo run --release`.

## Security

- A MoonProto key gives full control of its core. Only the bot's user can read `cores.json`
  (file 0600 in a 0700 folder). Without a [passphrase](#passphrase), the keys in it are plain text.
- With a passphrase, a copy of the disk, a snapshot or a backup does not show the keys.
  But a weak passphrase can be guessed, so use a long one.
  The passphrase does not help against someone who has root on the running server,
  because the unlocked keys are in the bot's memory.
- You send the passphrase through Telegram. Bot chats are not end-to-end encrypted,
  so Telegram's servers can see it. The bot deletes the message. It never writes the passphrase
  to the log or to disk.
- The bot never writes keys to the log. Telegram errors are logged without the request URL,
  because the URL contains the bot token.
- The bot works only in private chats, so you never send a key where other people can read it.
- Anyone can find a bot in Telegram. So only the one-time owner code or `OWNER_ID` can set the owner.
  The code has 12 random characters and is shown only in the server log. Wrong codes are logged.

## How it works

1. **Connect.** The bot connects to each core with the MoonProto SDK, using the core's key.
   It talks to the core directly over UDP. No other server is in between.
2. **Copy.** The first time, the bot downloads the core's `Orders` report and saves it in a local database.
   After that, the core sends new and changed trades live, and the bot updates its copy.
   If a core goes offline, the bot tries again every minute and catches up when the core is back.
3. **Count.** When you ask for a report, the bot adds up the closed trades from its local copy.
   It does not ask the core, so the answer is fast.
4. **Answer.** The bot gets your commands and button presses from Telegram and sends back the report
   as text or a table picture. With **Live trades** on, it also sends a message when a trade closes.

The MoonProto library is fixed to one commit in `Cargo.toml`.

## License

MIT, see [LICENSE](LICENSE). MoonProto is Apache-2.0. The JetBrains Mono font is under the SIL Open Font License, see [assets/fonts/OFL.txt](assets/fonts/OFL.txt).
Each release archive has a `THIRD_PARTY_LICENSES` file with the licenses of all the libraries and the font in the program.
It is made by [cargo-about](https://github.com/EmbarkStudios/cargo-about) from `about.toml` and `about.hbs`.
