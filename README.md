<p align="center">
    <img src="media/clauth.png" alt="clauth" width="480" />
</p>

[![Release](https://github.com/uwuclxdy/clauth/actions/workflows/release.yml/badge.svg)](https://github.com/uwuclxdy/clauth/actions/workflows/release.yml)
[![crates.io](https://img.shields.io/crates/v/clauth.svg)](https://crates.io/crates/clauth)

# Claude Code Account Switcher and Usage Monitor

A fast and simple Claude Code account switcher. Select a profile, confirm, done. Supports OAuth Claude Pro/Max/Team/Enterprise accounts as well as custom API profiles, so you can hop between accounts without logging in and out every time.

![alt text](media/demo.gif)

> Font is kinda off on the recording, I promise it looks better than this

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

- **One-key switching** — pick a profile, ⏎, confirm. Or `clauth <profile>` straight from the shell.
- **Automatic token refresh** — every profile's OAuth pair is refreshed in parallel on launch and on switch (same as Claude Code does on startup), and rotated when a 5-hour window expires, so usage queries never run with a stale token.
- **Live usage bars** — 5h utilization from the Anthropic API, refreshed every 45 seconds and color-coded with the next reset time. Max accounts also get a 7-day bar (Pro accounts don't have it in ther API respone).
- **Per-row activity** — each account shows a countdown to its next refresh or a color-coded spinner: sapphire fetch, cyan token refresh, orange switch, green auto-start.
- **Plan detection** — `/api/oauth/profile` identifies the tier: Pro, Max (5x / 20x), Team, Enterprise.
- **Per-account breakdown** — the Usage tab lays out every window (5h, 7d, 7d sonnet, 7d opus, any paid extra-usage spend) plus the endpoint, fallback threshold, and merged env keys.
- **Auto-switch on exhaustion** — opt accounts into an ordered fallback chain with per-profile thresholds; when the active one crosses its 5h limit (95% default), clauth hops to the next member with headroom. Needs clauth open.
- **Stale-data cues** — the account name underlines yellow when the row is served from cache and red when there's no data at all.
- **Account-change detection** — if Claude Code signed into a different account while clauth was closed, both the TUI and CLI notice on next launch and prompt `[Y/n]` before overwriting stored tokens.
- **Multi-instance safe** — several clauth processes coexist; state writes serialize through a file lock and each instance reloads when another rewrites `profiles.toml`. Switching, rotate-all, and refresh run off the UI thread.
- **Non-destructive** — only touches the API-related keys plus the profile's declared `env` block in `settings.json`; everything else is preserved.
- **Isolated launch** — `clauth start <profile> [claude args...]` runs `claude` in a per-call `CLAUDE_CONFIG_DIR` mirroring `~/.claude` via symlinks (copies on Windows without symlink privilege), with this profile's credentials, merged settings, and its own `.claude.json` — so cached account identity and billing caches don't leak between profiles.
- **Status-line aware** — `clauth which [--json]` prints which profile owns the loaded `credentials.json`.
- **Shell completions** — `clauth completions install [shell]` wires bash, zsh, or fish.
- **In-app help** — `?` opens a keybinding reference scoped to the current tab.

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

Preview the TUI with fake data for screenshots (dev-only, needs a checkout).
It's a `#[cfg(test)]` showcase, never built into the binary — it runs the real,
fully-interactive TUI (switch, edit, toggle, reorder all work) with the home dir
redirected to a throwaway tempdir, so nothing touches your real `~/.clauth` /
`~/.claude` and no network is used:

```bash
cargo test showcase -- --ignored --nocapture
```

The active profile is shown in orange. Each row's usage bars refreshes every 45 seconds and are cached locally so they stay visible even if the Anthropic API is rate-limited or offline. The 7-day bar is also shown when possible.

`Tab` / arrows move between the four tabs — **Overview** (switch and reorder accounts), **Usage** (per-account window breakdown), **Config** (edit endpoint, key, env, auto-start), and **Fallback** (chain editor). See hints at the bottom or `?` for more.

## Profile types

**Claude Pro / Max / Team / Enterprise (OAuth)** — leave base URL blank. clauth captures the OAuth token from your running session and restores it on switch. The plan tier (including Max 5x / Max 20x) is detected from the Anthropic API and shown in the list.

**API endpoint** — set a base URL and (optionally) an API key. Works with the official Anthropic API or any compatible proxy.

You can edit a profile's URL and key at any time without losing its stored credentials.

## Auto-starting the 5-hour timer

The 5-hour usage window only starts after a real inference call — the standard OAuth refresh that clauth runs on every launch doesn't trigger it. To make a profile's timer show up at clauth startup, toggle auto-start on the **Config** tab, or set it in `~/.clauth/profiles/<name>/config.toml`:

```toml
auto_start = true
```

When enabled, clauth sends a tiny Haiku ping (`max_tokens = 1`, fractions of a cent) for that profile on launch and on each refresh tick while there's no running window. Default is off. OAuth profiles only. The older field name `kick_timer = true` is still accepted on read.

## Automatic account switching

Open the **Fallback** tab to build an ordered list of profiles that clauth can hop between automatically when one runs out of 5-hour budget.

How it works:

- Each chain member has its own switch threshold (5h utilization %). Default is 95%; edit it inline on the Fallback tab (`+`/`-` to step, or type a value).
- After each usage refresh — at startup and on every per-profile tick while clauth is open — clauth checks the active profile. If it's a chain member and its 5h utilization is at or above its threshold, clauth walks the chain (starting at the slot after the active profile, wrapping) and switches to the first member whose own threshold hasn't been crossed. The active `◆` marker shifts to the new profile in place.
- A threshold of **100%** marks that profile as a last-resort sink. clauth still prefers any chain member with real headroom; only when every other member is past its threshold does it fall back to a 100%-threshold profile, even if that profile is itself capped. Claude Code will surface its own *"out of 5h limit"* message after the switch lands.
- A chain-global **wrap-off** toggle (also on the Fallback tab) decides what happens when every member is exhausted and no 100% sink exists: leave it off and clauth stays on the last account; turn it on and clauth switches off all accounts instead.
- If no chain member is available as a target, clauth stays put. If the active profile isn't in the chain, auto-switch is disabled.
- Profiles outside the chain are never auto-switched away from or auto-switched to — it's opt-in per profile.

Configuration lives in `~/.clauth/profiles.toml` (`fallback_chain` array, ordered) and per-profile `config.toml` (`fallback_threshold`). Both files are safe to edit by hand, but the Fallback tab is the easier path.

## Storage layout

```
~/.clauth/
  profiles.toml          # profile order, active marker, fallback chain, wrap-off, auto-start timestamps
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
