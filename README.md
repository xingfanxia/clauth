<p align="center">
    <img src="images/clauth.png" alt="clauth" width="480" />
</p>

[![Release](https://github.com/uwuclxdy/clauth/actions/workflows/release.yml/badge.svg)](https://github.com/uwuclxdy/clauth/actions/workflows/release.yml)
[![crates.io](https://img.shields.io/crates/v/clauth.svg)](https://crates.io/crates/clauth)

# Claude OAuth: clauth

A simple and fast Claude Code account switcher; select a profile, hit enter, done! Supports OAuth Claude Pro/Max accounts and API endpoint profiles.

```
? clauth
  ● work        Claude Max 5x  [████████░░] 82% · resets 2h 15m
    personal    Claude Pro     [███░░░░░░░] 28% · resets 4h 02m
    api-dev     https://api.notanthropic.com · API key set
  + New profile
  + New from current profile
    Quit
```

Claude Code stores session state in two places: `~/.claude/.credentials.json` and the `env` block inside `~/.claude/settings.json` (base URL and API key). Switching accounts means editing both by hand, every time.

clauth keeps snapshots of both files for each profile; on switch it swaps `.credentials.json` and updates only the API variables in the `env` block of `.settings.json` — everything else stays intact.

## Features

- **One-key switching:** select a profile, switch, done; or `clauth <profile>` to switch directly by profile name
- **Usage bar:** live 5-hour utilization fetched from the Anthropic API and refreshed every 30s, color-coded by threshold, with the next reset time alongside the bar
- **Plan detection:** queries `/api/oauth/profile` to identify the real plan tier — Pro, Max 5x, Max 20x, Team, Enterprise, Free — instead of trusting the unreliable `subscriptionType` tag in the OAuth credentials
- **Detailed window stats:** the per-profile submenu also shows the 7-day rolling window and any paid extra-usage spend
- **Non-destructive:** only touches the two API-related keys in `settings.json`; all other config is preserved

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

## Updates

Binary installs check for updates silently in the background each time clauth starts. The updated binary takes effect on the next run. No action needed.

Cargo installs: `cargo install clauth` to upgrade.

## Quickstart

Capture your current Claude Code session as a profile:

```bash
clauth
# Select "+ New from current profile"
# Enter a name, e.g. "work"
```

Create a second profile while logged in to a different account, then switch between them:

```bash
clauth
# Select the profile → "Switch to this profile"
```

Or switch directly by name — no menu, no prompts:

```bash
clauth work
# switched to 'work'
```

The active profile is marked with `●`. The 5-hour usage bar refreshes every 30 seconds and is cached locally so it stays visible even if the Anthropic API is rate-limited or offline.

## Profile types

**Claude Pro / Max / Team / Enterprise (OAuth)** — leave base URL blank. clauth captures the OAuth token from your running session and restores it on switch. The actual plan tier (including Max 5x / Max 20x) is detected from the Anthropic API and shown in the list.

**API endpoint** — set a base URL and (optionally) an API key. Works with the official Anthropic API or any compatible proxy.

You can edit a profile's URL and key at any time without losing its stored credentials.

## Kicking the 5-hour timer

The 5-hour usage window only starts after a real inference call — OAuth token refresh alone doesn't trigger it. To make a profile's timer show up at clauth startup, opt in per-profile by setting `kick_timer = true` in `~/.clauth/profiles/<name>/config.toml`:

```toml
kick_timer = true
```

When enabled, clauth refreshes the OAuth token and sends a 1-token Haiku ping (~22 input + 1 output token, fractions of a cent) for that profile on startup if no window is currently running. Default is off.

## Storage layout

```
~/.clauth/
  profiles.toml          # profile order + active marker
  profiles/
    work/
      config.toml        # base_url, api_key, kick_timer
      credentials.json   # OAuth token snapshot
      usage_cache.json   # last known utilization + plan info
    personal/
      ...
```

## Contributing

Bug reports and pull requests welcome. Run `cargo fmt` before submitting.

## License

MIT
