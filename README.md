<p align="center">
    <img src="media/clauth.png" alt="clauth — Claude Code account switcher and usage monitor TUI" width="480" />
</p>

[![Release](https://github.com/uwuclxdy/clauth/actions/workflows/release.yml/badge.svg)](https://github.com/uwuclxdy/clauth/actions/workflows/release.yml)
[![crates.io](https://img.shields.io/crates/v/clauth.svg)](https://crates.io/crates/clauth)

# Claude Code Account Switcher and Usage Monitor

**clauth** is a terminal UI to **switch between multiple Claude Code accounts** without logging out, and to monitor Claude Code usage and rate limits in real time. It handles Claude Pro / Max / Team / Enterprise OAuth accounts and custom API endpoints, auto-switches to a fallback account when you hit the 5-hour limit, and runs parallel Claude Code sessions under different accounts. Linux, macOS, Windows.

![clauth TUI demo — switching Claude Code accounts with live usage bars](media/demo.gif)

> Font is kinda off on the recording, I promise it looks better than this

## How it works

Claude Code keeps session state in `~/.claude/.credentials.json` (OAuth tokens) and the `env` block of `~/.claude/settings.json` (base URL, API key). clauth snapshots both per profile. On switch it swaps `.credentials.json` and the API env keys in place and leaves everything else untouched. `clauth start` instead launches `claude` in a temporary `~/.claude` mirrored via symlinks, so multiple accounts run simultaneously.

## Install

Supported platforms: Linux, macOS, Windows (Git Bash / MSYS2).

**Via cargo** (recommended):

```bash
cargo install clauth
```

**Via install script** (no Rust toolchain required; `--nocargo` forces a binary download):

```bash
curl -fsSL https://raw.githubusercontent.com/uwuclxdy/clauth/mommy/install.sh | bash
```

**Build from source:**

```bash
git clone https://github.com/uwuclxdy/clauth
cd clauth
cargo build --release
# binary at ./target/release/clauth
```

Binary installs update themselves in the background; cargo installs upgrade with `cargo install clauth`. Every install and update path checks a checksum and signature before it runs, and `CLAUTH_NO_UPDATE=1` turns updates off. Details in [SECURITY.md](SECURITY.md).

On first TUI launch, clauth offers to install shell completions. It asks before touching your shell rc, and `CLAUTH_NO_COMPLETIONS=1` skips it. Re-run any time with `clauth completions install [shell]`.

## Features

- **One-key switching**: pick a profile, ⏎, confirm. Or `clauth <profile>` straight from the shell.
- **Automatic token refresh**: OAuth refresh tokens are single-use, so rotation is lazy. A stale access token rotates the moment a usage query 401s, never proactively. `t` force-rotates every account.
- **Live usage bars**: 5h utilization from the Anthropic API on a configurable interval (default 90 s), color-coded with the next reset time. Max accounts also get a 7-day bar.
- **Per-row activity**: countdown to the next refresh, or a color-coded spinner (sapphire fetch, cyan token refresh, green auto-start).
- **Plan detection**: Pro, Max (5x / 20x), Team, Enterprise, identified via `/api/oauth/profile`.
- **Per-account breakdown**: the Usage tab lays out every window (5h, 7d, 7d sonnet, 7d opus, paid extra-usage spend) plus endpoint, fallback threshold, and merged env keys.
- **Auto-switch on exhaustion**: opt accounts into an ordered fallback chain. When the active one crosses its 5h threshold (95% default), clauth hops to the next member with headroom. Needs clauth open.
- **Stale-data cues**: account name underlines yellow when served from cache, red when there's no data.
- **Account-change detection**: if Claude Code signed into a different account while clauth was closed, you get a `[Y/n]` prompt before stored tokens are overwritten.
- **Multi-instance safe**: state writes serialize through a file lock, each instance reloads on external changes, and HTTP runs off the UI thread.
- **Non-destructive**: only the API keys plus the profile's declared `env` block in `settings.json` are touched.
- **Isolated launch**: `clauth start <profile> [claude args...]` runs `claude` in a per-call `CLAUDE_CONFIG_DIR` (symlink mirror; copies on Windows without symlink privilege), so account identity and billing caches never leak between profiles.
- **Status-line aware**: `clauth which [--json]` prints which profile owns the loaded `credentials.json`.
- **Shell completions**: `clauth completions install [shell]` wires bash, zsh, or fish.
- **In-app help**: `?` opens a keybinding reference scoped to the current tab.
- **Claude status feed**: the Status tab pulls live incidents from status.claude.com, with per-component health (claude.ai, API, Claude Code, Cowork), severity, and timeline, cached to disk.

## Quickstart

Capture your current Claude Code session as a profile:

```bash
clauth
# Select "+ new from current profile", enter a name, e.g. "work"
```

Repeat while logged in to a different account, then switch in the TUI (⏎ + confirm) or directly by name:

```bash
clauth work
# switched to 'work'
```

Run claude under a profile without touching the global config:

```bash
clauth start personal -- --model haiku
# spawns claude with personal's credentials in an isolated CLAUDE_CONFIG_DIR
```

The active profile is shown in orange. Usage bars are cached locally, so they stay visible even if the Anthropic API is rate-limited or offline. `←`/`→` move between the six tabs: **Overview** (switch and reorder accounts), **Usage** (per-account window breakdown), **Setup** (endpoint, key, env, auto-start), **Fallback** (chain editor), **Config** (theme, refresh interval, wrap-off, divergence default), and **Status** (Claude incident feed). `?` shows the rest.

Dev-only: `cargo test showcase -- --ignored --nocapture` runs the real interactive TUI on fake data against a throwaway home dir (never built into the binary, no network). Handy for screenshots.

## Profile types

**Claude Pro / Max / Team / Enterprise (OAuth)** — leave base URL blank. clauth captures the OAuth token from your running session and restores it on switch; the plan tier is detected automatically.

**API endpoint** — set a base URL and (optionally) an API key. Works with the official Anthropic API or any compatible proxy. URL and key are editable any time without losing stored credentials.

## Auto-starting the 5-hour timer

The 5-hour usage window only starts after a real inference call. The OAuth refresh clauth runs at launch doesn't trigger it. To arm a profile's timer at clauth startup, toggle auto-start on the **Setup** tab, or set it in `~/.clauth/profiles/<name>/config.toml`:

```toml
auto_start = true
```

When enabled, clauth sends a tiny Haiku ping (`max_tokens = 1`, fractions of a cent) on launch and on each refresh tick while there's no running window. On a cold start it fetches usage before the first kick, so it never pings blind over a window that might already be live; the timer can arm one tick late as a result. Default off, OAuth profiles only. The older field name `kick_timer = true` is still accepted on read.

The ping is a real, billed `/v1/messages` call under your own OAuth token, the same request Claude Code fires on startup (see [what acts on your behalf](SECURITY.md#what-acts-on-your-behalf)). Leave auto-start off if you'd rather only the live `claude` process open a window.

## Automatic account switching

The **Fallback** tab holds an ordered chain of profiles that clauth hops between when one runs out of 5-hour budget:

- Each member has its own threshold (5h utilization %, default 95%); edit inline (`+`/`-` or type).
- After each usage refresh (at startup and on every tick), if the active profile is a chain member at or above its threshold, clauth walks the chain (wrapping) and switches to the first member under its own threshold. The `◆` marker shifts in place.
- A **100%** threshold marks a last-resort sink: chosen only when every other member is past its threshold. Claude Code surfaces its own *"out of 5h limit"* message after the switch lands.
- The chain-global **wrap-off** toggle (Config tab) decides what happens when everyone is exhausted and no sink exists: off = stay on the last account; on = switch off all accounts, then re-arm automatically once any member drops back under its threshold.
- No eligible target → clauth stays put. Active profile not in the chain → auto-switch disabled. Profiles outside the chain are never switched away from or to. It's opt-in.

Configuration lives in `~/.clauth/profiles.toml` (`fallback_chain`, ordered) and per-profile `config.toml` (`fallback_threshold`); both are safe to hand-edit.

## Storage layout

```
~/.clauth/
  profiles.toml          # profile order, active marker, fallback chain, wrap-off, theme, refresh interval
  profiles/
    work/
      config.toml        # base_url, api_key, auto_start, fallback_threshold, [env]
      credentials.json   # OAuth token snapshot (credentials.json.pending while a rotation is mid-write)
      usage_cache.json   # last known utilization + plan info
      runtime/           # isolated CLAUDE_CONFIG_DIR tree for `clauth start`
      sessions/          # per-session PID files (ref-counting live launches)
    personal/
      ...
```

## Alternatives

Most tools in this space do one half. clauth does both in one TUI, switching *and* live usage, connected by the auto-switch chain.

| Tool | What it does | Compared to clauth |
|------|--------------|--------------------|
| [claude-swap](https://github.com/realiti4/claude-swap) | CLI account switcher (token backup/restore) | no usage view, no auto-switch |
| [CCSwitcher](https://github.com/XueshiQiao/CCSwitcher), [claude-account-switcher](https://github.com/Symbioose/claude-account-switcher) | macOS menu-bar switchers | macOS-only, no fallback chain |
| [cc-account-switcher](https://github.com/ming86/cc-account-switcher) | credential-swap scripts | no TUI, no usage |
| [Claude-Code-Usage-Monitor](https://github.com/Maciek-roboblog/Claude-Code-Usage-Monitor) | real-time usage monitor with predictions | monitoring only, single account |
| [claude-code-statusline](https://github.com/ohugonnot/claude-code-statusline) | rate-limit status line inside Claude Code | in-session display, no switching |
| `CLAUDE_CONFIG_DIR` by hand | manual per-account config dirs | what `clauth start` automates |

## FAQ

**How do I switch between multiple Claude Code accounts without logging out?**
Install clauth, save each logged-in session as a profile once, then switch with `clauth <name>` or a single keypress in the TUI. No browser, no re-login.

**Can I run Claude Code with multiple accounts at the same time?**
Yes. `clauth start <profile>` launches `claude` in an isolated `CLAUDE_CONFIG_DIR`, so parallel sessions don't share identity, settings, or billing caches.

**Can Claude Code switch accounts automatically when I hit the 5-hour limit?**
With clauth open, yes: put accounts in the fallback chain and clauth switches to the next member with headroom the moment the active one crosses its threshold.

**How do I monitor Claude Code usage and rate limits?**
The Overview tab shows color-coded 5h (and 7-day) bars per account with reset times; the Usage tab breaks down every rate-limit window the API reports.

**Does it work with Claude Pro, Max, Team, and Enterprise?**
Yes. OAuth profiles cover all paid tiers (plan auto-detected, including Max 5x / 20x). API-endpoint profiles cover the Anthropic API or any compatible proxy.

**Where does clauth store my Claude Code credentials?**
Locally under `~/.clauth/`, with `0600` permissions on Unix. Tokens are only ever sent to Anthropic. See [SECURITY.md](SECURITY.md) for the full breakdown.

## Security

clauth handles live OAuth tokens and replaces its own binary over the network, so [SECURITY.md](SECURITY.md) lays out the trust model: where credentials live, every host clauth contacts, how updates get verified, and how to switch each behavior off. Found something exploitable? Report it privately through the repo's **Security → Report a vulnerability**.

## License
MIT