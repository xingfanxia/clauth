# Security

clauth keeps live Claude Code OAuth tokens on disk and replaces its own binary over
the network. Both are worth scrutiny, so this doc spells out what it stores, what it
talks to, what can touch your account, how releases get verified, and how to switch
each thing off.

## Reporting a vulnerability

Report privately through GitHub: open the repo's **Security** tab and pick **Report a
vulnerability**. Please don't file a public issue for anything exploitable. A
description, the affected version, and repro steps are enough to get started.

## Supported versions

Only the latest release. Binary installs stay current through the verified
auto-updater below; `cargo` installs update with `cargo install clauth`.

## Data at rest

Per-profile state lives under `~/.clauth/`. Credentials are stored as tightly as the
OS allows.

| Path | Contents | Unix mode |
|------|----------|-----------|
| `~/.clauth/profiles/<name>/credentials.json` | OAuth token snapshot | file `0600`, dirs `0700` |
| `~/.clauth/profiles/<name>/config.toml` | base URL, API key (endpoint profiles), env block | `0600` |
| `~/.clauth/profiles/<name>/usage_cache.json` | last-known utilization and plan | `0600` |
| provider stats cache (`~/.clauth/`) | third-party account state | file `0600`, dir `0700` |

- Writes are atomic. The temp file gets mode `0600` at creation, not a chmod
  afterward, so a loose umask never leaves a readable window; it's fsynced, then
  renamed into place. A rotation caught mid-write lands as `credentials.json.pending`
  and is promoted only once it's durable.
- Modes are enforced on Unix. On Windows, access falls to the default user-profile
  ACLs, which clauth does not loosen.
- A switch rewrites two things: `~/.claude/.credentials.json` and the `env` block of
  `~/.claude/settings.json`. The rest of `~/.claude/` is left alone.

## Network activity

Every request clauth makes, and what rides along with it:

| Endpoint | When | Carries |
|----------|------|---------|
| `api.github.com/repos/uwuclxdy/clauth/releases/latest` + release assets | background update check on launch (binary installs only) | no credentials, just a `User-Agent` |
| `api.anthropic.com/v1/oauth/token` | lazy token refresh (on a 401) and `t` force-rotate | your stored refresh token |
| `claude.com/cai/oauth/authorize` | `clauth login` interactive sign-in, opened in your browser | no credentials; a PKCE challenge + random `state` |
| `platform.claude.com/v1/oauth/token` | `clauth login` authorization-code exchange | the one-time auth code + PKCE verifier (mints a fresh token pair) |
| `api.anthropic.com/api/oauth/usage` | usage poll on the refresh interval | access token (Bearer) |
| `api.anthropic.com/api/oauth/profile` | plan-tier detection, and reading which account a token belongs to (so a live re-login can be told apart) | access token |
| `api.anthropic.com/v1/messages` | auto-start kick (opt-in, off by default) | access token; a 1-token Haiku request |
| `status.claude.com/api/v2/incidents.json` | Status tab and background poll | no credentials |
| `raw.githubusercontent.com/BerriAI/litellm/...` | model price table for the Tokens tab cost lens, fetched and disk-cached | no credentials |
| `api.deepseek.com/user/balance` | only for profiles whose base URL is DeepSeek | that provider's API key |
| a custom base URL you set | requests against an API-endpoint profile | whatever you configured |

Your stored access/refresh tokens go to `api.anthropic.com` and nowhere else. The only
exception is the interactive `clauth login`, which follows Claude Code's own OAuth flow:
it opens `claude.com` in your browser to authorize and posts the one-time authorization
code to `platform.claude.com` to mint the new profile's token pair. clauth runs no
telemetry or analytics; it talks to the hosts above and no others.

## What acts on your behalf

A few code paths can change account state. All are narrow and all are documented.

Background, automatic:

- **Auto-start kick.** A real, billed `/v1/messages` call (`max_tokens = 1`, a
  fraction of a cent) under your own OAuth token, with the Claude Code client
  identity. It's the same request Claude Code makes on startup, and it exists to arm
  the 5-hour usage window. Off by default, OAuth profiles only; enable it per profile
  on the Setup tab or with `auto_start = true`.
- **Token refresh.** Anthropic refresh tokens are single-use, so refreshing spends
  the stored token for a fresh pair. It's lazy: it fires only when a usage query
  returns 401, never ahead of time, unless you press `t` to force it.

User-invoked, only when you run the command:

- **Interactive login (`clauth login <profile>`).** Opens your browser to Claude's
  OAuth authorize page and binds a loopback listener on `127.0.0.1:<random port>` to
  catch the redirect, then exchanges the returned code for a fresh token pair written
  into the new profile. It reproduces Claude Code's own PKCE flow, touches no other
  account, and never opens a usage window. On macOS this is why `clauth login` works at
  all — Claude Code's own `/login` under a custom config dir writes only a per-config-dir
  Keychain item, never the profile's credentials file.

Agent-invoked, only when the Claude Code plugin is installed:

- **`delegate` (MCP tool).** Sends a real, billed `/v1/messages` request on a target
  profile under its own OAuth token, opening a full 5-hour usage window on that
  account. It fires only when an agent calls the tool, and is hard-capped at
  recursion depth 1 (a delegated session cannot call `delegate` again).
- **`switch` (MCP tool).** Relinks the global `~/.claude` credentials to another
  profile, the same write `clauth switch` performs. It changes which account the
  global session refreshes onto; it sends no inference itself.

Nothing else sends inference or writes to your account.

## Auto-update verification

Binary installs check for a newer release in the background on launch. Every step
fails closed, so if any of them errors the update is skipped and the running binary
stays put:

1. Ask the GitHub releases API for the latest tag; stop if it isn't newer.
2. Download `sha256sums.txt`. A fetch error stops here (no integrity, no update).
3. Download `sha256sums.txt.minisig` and check it against a minisign public key
   pinned at compile time. A missing or bad signature stops the update. The key is a
   constant, so nothing at runtime can swap it out.
4. Download the platform asset (10 MB ceiling) and check its SHA-256 against the
   now-trusted sums file. A mismatch stops the update.
5. Write to a temp file, fsync, then self-replace atomically. The new binary takes
   over on the next launch.

`cargo` installs (binary under `~/.cargo/bin`) are told an update exists but never
replaced. `CLAUTH_NO_UPDATE=1` turns the whole thing off.

Releases are signed in CI with a passwordless minisign key kept as a GitHub Actions
secret; the signing step writes the key to disk and deletes it on exit. The public
half is pinned in `src/update.rs`.

## Install-script verification

`install.sh` (the `curl | bash` path) uses `cargo` when it's available. When it pulls
a prebuilt binary instead, it downloads `sha256sums.txt` from the same release and
checks the binary against it before installing, failing closed on a download or
checksum error. It writes nothing to your shell profile and only prints a `PATH` hint
when the install dir isn't already on it. If piping a script to a shell isn't your
thing, `cargo install clauth` does the same job.

## Process execution

- `clauth start <profile> [claude args...]` runs `claude` (found on `PATH`) with
  `CLAUDE_CONFIG_DIR` pointed at the profile's isolated runtime, forwarding your extra
  args. Args go through an argument vector, never a shell, so there's no
  shell-injection path.
- On Linux, opening a status-incident link runs `xdg-open <url>` with null stdio. The
  URL comes from the Statuspage feed and is passed as a single argument.
- clauth runs no other external commands.

## First-run shell completions

On the first TUI launch clauth offers to install shell completions. For bash and zsh
it asks before adding a `source` line to your rc file (`[Y/n]`, interactive sessions
only); fish gets its own completions dir. The answer is saved to
`~/.clauth/.completions_installed` so the prompt doesn't come back.
`CLAUTH_NO_COMPLETIONS=1` skips it.

## Build and supply chain

- `unsafe` is denied across the crate (`unsafe_code = "deny"`,
  `unsafe_op_in_unsafe_fn = "deny"`).
- CI runs `cargo fmt --check`, `cargo clippy --all-targets --all-features -D warnings`,
  and the test suite (`--all-features`) on Linux, macOS, Windows for every push and PR.
- `cargo-deny` (advisories denied by default, license allowlist, sources locked to
  crates.io, `openssl` banned in favor of rustls) and `cargo-audit` both run in CI.
- `Cargo.lock` is committed and dependency versions are pinned.

## Switching behaviors off

| Switch | Effect |
|--------|--------|
| `CLAUTH_NO_UPDATE=1` | disables all background update checks and self-replacement |
| `CLAUTH_NO_COMPLETIONS=1` | skips the first-run completion-install prompt |
| `install.sh --nocargo` | forces a verified binary download instead of `cargo install` |
| `cargo install` | never self-replaces; update with `cargo install clauth` |
