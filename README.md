<p align="center">
    <img src="images/clauth.png" alt="clauth" width="480" />
</p>

[![Release](https://github.com/uwuclxdy/clauth/actions/workflows/release.yml/badge.svg)](https://github.com/uwuclxdy/clauth/actions/workflows/release.yml)
[![crates.io](https://img.shields.io/crates/v/clauth.svg)](https://crates.io/crates/clauth)

# Claude Code Account Switcher and Usage Monitor

A fast and simple Claude Code account switcher. Select a profile, confirm, done. Supports OAuth Claude Pro/Max/Team/Enterprise accounts as well as custom API profiles, so you can hop between accounts without logging in and out every time.

```
 ▐▛███▜▌  clauth
▝▜█████▛▘ OVERVIEW  active: work · 3 accounts
  ▘▘ ▝▝

    account     type            5h                        7d             route
  ▸ ◆ work      Max 5x      21s [████████░░] 82% (2h 5m)  12% (1d 6h)    #1 @ 95%
    ◇ personal  Pro          ⠷  [███░░░░░░░] 28% (4h)     —              —
    ◇ api-dev   API          —  —                         —              —

    ACTIONS
    + new profile
    + new from current profile
    → fallback chain

  FALLBACK · flow
   1 ◆ work · 82/95%  ─▶  2 ◇ personal · 28/95%   ↺

  ⏎ switch   m menu   d details   f chain   r refresh   t rotate all   ? help   q quit
```

Claude Code keeps session state in `~/.claude/.credentials.json` (OAuth tokens) and the `env` block of `~/.claude/settings.json` (base URL, API key). Without clauth, switching accounts means logging out and back in. Why go through all that when:
1. clauth can keep a snapshot of both files per profile,
2. on switch it swaps `.credentials.json` and updates the API variables (plus the profile's own `env` block) in `settings.json` — everything else stays intact,
3. or launches `claude` in a temporary `~/.claude` directory, connected to the main one via symlinks, with the profile's credentials and merged env vars, so you can run Claude Code with multiple accounts simultaneously.

## Install

Supported platforms:
- Linux
- macOS
- Windows (Git Bash / MSYS2)

**Via cargo** (recommended):

```bash
cargo install clauth
```

---

**Via install script** (no Rust toolchain required):

```bash
curl -fsSL https://raw.githubusercontent.com/uwuclxdy/clauth/mommy/install.sh | bash
```

Pass `--nocargo` to force a binary download even when cargo is available:

```bash
curl -fsSL https://raw.githubusercontent.com/uwuclxdy/clauth/mommy/install.sh | bash -s -- --nocargo
```

---

**Build from source:**

```bash
git clone https://github.com/uwuclxdy/clauth
cd clauth
cargo build --release
# binary at ./target/release/clauth
```

## Features

- **One-key switching:** select a profile, press enter and confirm. Or `clauth <profile>` to switch directly from the shell.
- **Automatic token refresh:** every profile's OAuth pair is refreshed in parallel on launch and on manual switch — matching what Claude Code does silently on startup — so usage queries never run with an expired access token. Tokens are also rotated when a 5-hour window expires, with usage re-fetched immediately.
- **Usage bars:** live 5-hour utilization fetched from the Anthropic API on a per-profile adaptive cadence (baseline 35s, floor 10s, ceiling 300s — backs off on rate limits, recovers on quiet periods), color-coded by threshold, with the next reset time alongside the bar. Max accounts also get a 7-day bar when the terminal is wide enough; Pro accounts have no weekly window in the API response so only the 5h bar is shown.
- **Per-profile activity indicator:** each row carries its own countdown (`Ns` until next refresh) or animated spinner color-coded by what's happening — sapphire for fetch, cyan for token refresh, orange for switch, green for auto-start.
- **Plan detection:** queries `/api/oauth/profile` to identify the plan tier — Pro, Max 5x, Max 20x, Team, Enterprise, Free.
- **Per-profile detail screen:** `d` opens a side-by-side breakdown of every usage window (5h, 7d all, 7d sonnet, 7d opus, any paid extra-usage spend) plus the profile's endpoint, fallback threshold, and merged env keys.
- **Auto-switch on exhaustion:** opt profiles into an ordered fallback chain with per-profile thresholds; when the active profile crosses its 5h limit (95% by default), clauth switches to the next chain member that still has headroom. clauth must be open for this to work.
- **Stale-data indicator:** the profile name is underlined yellow when the usage row is served from cache (API refused this tick) and red when no data is available.
- **Account-change detection:** if Claude Code signed into a different account while clauth wasn't running, both the TUI and CLI switch path notice on next launch and prompt `[Y/n]` (naming outgoing + target) before overwriting the active profile's stored tokens.
- **Multi-instance safe:** several clauth processes can run side by side; state writes are serialized through a file lock and each instance reloads when another rewrites `profiles.toml`. Account switching, rotate-all, and token refresh run off the UI thread, with switches blocked while any profile is mid-operation.
- **Non-destructive:** only touches the API-related keys plus the profile's declared env block in `settings.json`; all other config is preserved.
- **Isolated launch:** `clauth start <profile> [claude args...]` spawns `claude` in a per-call `CLAUDE_CONFIG_DIR` that mirrors `~/.claude` via symlinks (or copies on Windows without symlink privilege), with this profile's credentials, merged settings, and its own `.claude.json` so Claude Code's cached account identity and billing caches don't leak between profiles.
- **Status-line aware:** `clauth which [--json]` prints the profile that owns the loaded `credentials.json` (honors `CLAUDE_CONFIG_DIR`).
- **Shell completions:** `clauth completions install [shell]` wires bash, zsh, or fish completion for profile names and subcommands.
- **In-app help:** `?` from any screen opens a context-aware keybinding reference.

## Updates

Binary installs automatically update in the background; new binary takes effect on the next run. Cargo installs: `cargo install clauth` to upgrade.

## Quickstart

Capture your current Claude Code session as a profile:

```bash
clauth
# Select "+ new from current profile"
# Enter a name, e.g. "work"
```

Create a second profile while logged in to a different account, then switch between them:

```bash
clauth
# Move the cursor onto a profile, press ⏎ and confirm
```

Or switch directly by name — no menu, no prompts:

```bash
clauth work
# switched to 'work'
```

Run claude under a profile without touching the global config:

```bash
clauth start personal -- --model haiku
# spawns claude with personal's credentials in an isolated CLAUDE_CONFIG_DIR
```

Preview the TUI with fake data for screenshots (dev-only, needs a checkout; no
network, no config changes). It's a `#[cfg(test)]` showcase, never built into
the binary:

```bash
cargo test showcase -- --ignored --nocapture
```

The active profile is shown in orange. Each row's usage bars refresh on an adaptive per-profile schedule (10–300s, baseline 35s; the row's timer counts down to the next tick) and are cached locally so they stay visible even if the Anthropic API is rate-limited or offline. The 7-day bar is appended only when the terminal is wide enough. Tabs switch between overview, usage, config, and the fallback chain editor; `n` adds an account, `t` rotates every profile's tokens at once, `?` opens help.

## Profile types

**Claude Pro / Max / Team / Enterprise (OAuth)** — leave base URL blank. clauth captures the OAuth token from your running session and restores it on switch. The plan tier (including Max 5x / Max 20x) is detected from the Anthropic API and shown in the list.

**API endpoint** — set a base URL and (optionally) an API key. Works with the official Anthropic API or any compatible proxy.

You can edit a profile's URL and key at any time without losing its stored credentials.

## Auto-starting the 5-hour timer

The 5-hour usage window only starts after a real inference call — the standard OAuth refresh that clauth runs on every launch doesn't trigger it. To make a profile's timer show up at clauth startup, toggle `auto_start` from the profile menu (`m`), or set it in `~/.clauth/profiles/<name>/config.toml`:

```toml
auto_start = true
```

When enabled, clauth sends a 1-token Haiku ping (~22 input + 1 output token, fractions of a cent) for that profile on launch and on each refresh tick while there's no running window. Default is off. OAuth profiles only. The older field name `kick_timer = true` is still accepted on read.

## Automatic account switching

Open the **fallback chain** entry in the main menu (`f`) to build an ordered list of profiles that clauth can hop between automatically when one runs out of 5-hour budget.

How it works:

- Each chain member has its own switch threshold (5h utilization %). Default is 95%; edit it from the chain entry's submenu.
- After each usage refresh — at startup and on every per-profile tick while clauth is open — clauth checks the active profile. If it's a chain member and its 5h utilization is at or above its threshold, clauth walks the chain (starting at the slot after the active profile, wrapping) and switches to the first member whose own threshold hasn't been crossed. The active `◆` marker shifts to the new profile in place.
- A threshold of **100%** marks that profile as a last-resort slot. clauth still prefers any chain member with real headroom; only when every other member is past its threshold does it fall back to a 100%-threshold profile, even if that profile is itself capped. Claude Code will surface its own *"out of 5h limit"* message after the switch lands.
- If no chain member is available as a target, clauth stays put. If the active profile isn't in the chain, auto-switch is disabled.
- Profiles outside the chain are never auto-switched away from or auto-switched to — it's opt-in per profile.

Configuration lives in `~/.clauth/profiles.toml` (`fallback_chain` array, ordered) and per-profile `config.toml` (`fallback_threshold`). Both files are safe to edit by hand, but the menu is the easier path.

## Storage layout

```
~/.clauth/
  profiles.toml          # profile order, active marker, fallback chain, auto-start timestamps
  profiles/
    work/
      config.toml        # base_url, api_key, auto_start, fallback_threshold, [env]
      credentials.json   # OAuth token snapshot
      usage_cache.json   # last known utilization + plan info
    personal/
      ...
```

---

I'll do `claude "good boy"` for every star ts gets.
