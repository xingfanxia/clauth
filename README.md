<p align="center">
    <img src="images/clauth_banner.png" alt="clauth" width="480" />
</p>

[![Release](https://github.com/uwuclxdy/clauth/actions/workflows/release.yml/badge.svg)](https://github.com/uwuclxdy/clauth/actions/workflows/release.yml)
[![crates.io](https://img.shields.io/crates/v/clauth.svg)](https://crates.io/crates/clauth)

---

A simple Claude Code account switcher. Select a profile, hit enter, done. Supports OAuth Claude Pro/Max accounts and API endpoint profiles.

```
? clauth
  ● work        Claude Pro   [████████░░] 94%
    personal    Claude Max   [███░░░░░░░] 28%
    api-dev     https://api.notanthropic.com · API key set
  + New profile
  + New from current profile
    Quit
```

Claude Code stores session state in two places: `~/.claude/.credentials.json` and the `env` block inside `~/.claude/settings.json` (base URL and API key). Switching accounts means editing both by hand, every time.

clauth keeps snapshots of both files for each profile; on switch it swaps `.credentials.json` and updates only the API variables in the `env` block of `.settings.json` — everything else stays intact.

## Features

- **One-key switching** — select a profile, switch, done; or `clauth <profile>` to switch directly by profile name
- **5-hour usage bar** — live utilization fetched from the Anthropic API at startup, color-coded by threshold
- **Subscription type detection** — reads `subscriptionType` from each profile's credentials and displays it (Pro, Max, etc.)
- **Auto-update** — binary installs silently update themselves in the background
- **Non-destructive** — only touches the two API-related keys in `settings.json`; all other config is preserved

## Install

Supported platforms: Linux · macOS · Windows (Git Bash / MSYS2)

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

The active profile is marked with `●`. The 5-hour usage bar updates on each launch and is cached locally so it stays visible even when the Anthropic API is rate-limited.

## Profile types

**Claude Pro / Max (OAuth)** — leave base URL blank. clauth captures the OAuth token from your running session and restores it on switch. The subscription tier is read directly from the token and shown in the list.

**API endpoint** — set a base URL and (optionally) an API key. Works with the official Anthropic API or any compatible proxy.

You can edit a profile's URL and key at any time without losing its stored credentials.

## Storage layout

```
~/.clauth/
  profiles.toml          # profile order + active marker
  profiles/
    work/
      config.toml        # base_url, api_key
      credentials.json   # OAuth token snapshot
      usage_cache.json   # last known 5-hour utilization
    personal/
      ...
```

## Contributing

Bug reports and pull requests welcome. Run `cargo fmt` before submitting.

## License

MIT
