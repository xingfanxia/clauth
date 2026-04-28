# clauth

A simple Claude Code account switcher. Select a profile, hit enter, done. Supports OAuth Claude Pro/Max accounts and API endpoint profiles.

```
? clauth
  ● work        Claude Pro / OAuth
    personal    Claude Pro / OAuth
    api-dev     https://api.notanthropic.com · API key set
  + New profile
  + New from current profile
    Quit
```

Claude Code stores session in two places: `~/.claude/.credentials.json` and the `env` block inside `~/.claude/settings.json` (base URL and API key). Switching accounts means editing both by hand, every time.

clauth keeps snapshots of both files for each profile; on change it swaps the `.credentials.json` and changes only the API variables in the `env` block of `.settings.json` - everything else stays intact.

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

---

The script detects `cargo` and uses it when available. Pass `--nocargo` to force a binary download instead:

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

Binary installs (non-cargo) check for updates silently in the background each time clauth starts. The updated binary takes effect on the next run. No action needed.

Cargo updates clauth via `cargo update`.

## Quickstart

Capture your current Claude Code session:

```bash
clauth
# Select "+ New from current profile"
# Enter a name, e.g. "work"
```

Create a second profile while logged in to a different account, then switch between them with:

```bash
clauth
# Select the profile → "Switch to this profile"
```

The active profile is marked with `●` in the list.

## Profile types

**Claude Pro (OAuth)** -- leave base URL blank. clauth captures the OAuth token from your running session and restores it on switch.

**API endpoint** -- set a base URL and (optionally) an API key. Works with the official Anthropic API or any compatible proxy.

You can edit a profile's URL and key at any time without losing its stored credentials. The "Edit" action in the submenu updates `config.toml` only.

## Storage layout

```
~/.clauth/
  profiles.toml          # profile order + active marker
  profiles/
    work/
      config.toml        # base_url, api_key
      credentials.json   # OAuth token snapshot
    personal/
      ...
```

## Contributing

Bug reports and pull requests welcome. Run `cargo fmt` before submitting.

## License

MIT
