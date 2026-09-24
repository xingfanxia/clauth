# Fork ↔ upstream sync contract

The fork (xingfanxia/clauth, branch `main`) tracks upstream (uwuclxdy/clauth,
branch `mommy`) by **periodic true git merges** — never squash-rebases, never
cherry-pick-only sweeps. Adopted 2026-07-20 (UPS-2, merge `c112469`); the
pre-history is one 0.12.0 squash baseline, so `git log --first-parent main`
reads as the fork's own timeline.

Why merges: history and ledger-cited hashes survive; PR #51's head IS this
branch, so a merge updates the PR without a force-push; each sync pays only the
incremental conflict cost. A squash-rebase re-pays the whole fork delta every
time and invalidates every hash `.agent/PROGRESS.md` and memory cite.

## Doing a sync

1. `git fetch upstream && git log --oneline $(git merge-base main upstream/mommy)..upstream/mommy`
   — read the delta first; know what's landing.
2. Branch: `git checkout -b sync/upstream-<date>`; merge: `git merge upstream/mommy`.
3. Resolve by the principles below. `cargo test` + `cargo clippy --all-targets`
   + `cargo fmt --check` must be green before the merge commit concludes.
4. Fast-forward `main` to the sync branch, deploy (daemon + proxy restart),
   push. PR #51 picks the merge up automatically.
5. Ledger the sync in `.agent/PROGRESS.md` (UPS-N) and update the fork-delta
   inventory below if it changed.
6. Audit by the INVENTORY, not by upstream's headlines: for every fork-delta
   line below, `git diff <old-merge-base> upstream/mommy` over the code that
   line touches and say what upstream changed around it. UPS-19 skipped this
   and shipped a bricking bug: upstream's new unmodelled-key carry re-wrote the
   fork's `session_feed` alias beside `rolling_token` (a duplicate field), so
   the first login after the deploy left `config.toml` unloadable. A green
   suite had no test combining the two; the inventory named the alias.
7. Exercise every WRITE path once after deploying, not only the read side:
   load and re-save each live `config.toml` and `profiles.toml` in a throwaway
   `HOME` copy (configs only, never credentials) and load the result.

## Resolution principles

1. **Divergence-reduction**: where both sides implement the same idea, take
   upstream's shape and re-express the fork delta on top of it. Every line the
   fork doesn't need to own is a line the next sync doesn't conflict on.
2. **Fork-only subsystems survive with behavior intact** (inventory below).
3. **Upstream-only features are adopted wholesale** — including their config
   and TUI surfaces — then gated through fork axes where an axis matters
   (e.g. settings sync skips codex-harness profiles).
4. **Hard-cap rule (the PR #55 bug class)**: the fork's `is_exhausted` /
   `weekly_blocked` FOLD per-member overrides; upstream's don't. Any upstream
   site judging the literal 100% cap through a folding predicate must be
   converted to `is_exhausted_hard` / `weekly_hard_blocked` / an explicit-line
   non-folding twin. Sweep for it on every sync:
   `grep -n 'is_exhausted(\|weekly_blocked(' src/ | grep WEEKLY_HARD_BLOCK_PCT`.
5. **Wire compatibility beats field names**: Rust fields follow upstream
   renames (`wrap_off` → `switch_off_when_spent`), but status.json keys and
   on-disk spellings ccsbar/ccu read stay stable (serde rename / literal key).
6. **Merge-both test hunks truncate**: when a `both` resolution concatenates
   two suites, the first side's last function often loses its tail before the
   second side's header. Every "unclosed delimiter" in the compile loop is
   this; restore the tail from `git show HEAD^1:<path>`.

## Fork-delta inventory (what upstream does not have)

- **Codex, on top of upstream's engine.** The fork's own codex engine
  (CDX-1..6: isolated homes, standby refresh, passive JSONL reader, the
  `clauth resume <codex-profile>` carryover, `CodexPollPacing`) was RETIRED in
  UPS-18 for upstream's #69, which the fork wrote. What the fork still owns:
  - the operator-link follow: a codex switch repoints `~/.codex/auth.json`
    when it is clauth's link into a store (`follow_operator_auth_slot`,
    `actions.rs`; upstream PR #91 proposes it);
  - codex routing through the fork's control socket and the per-harness drain
    (`socket.rs` `resolve`, `tick.rs` `drain_codex_switch`), and the forced
    codex poll a socket `refresh` asks for (`codex_poll_due`);
  - the localhost injection proxy (`src/proxy/*`); it no longer stands any
    usage leg down (the CDX-5 stand-down was removed in the UPS-19 audit);
  - the `codex_usage_poll` kill switch, honored by upstream's codex tick
    again since the UPS-19 audit;
  - `clauth migrate-codex`, the one-time move onto the two-file layout.
- **Scheduler**: the per-harness pending switch queue
  (`VecDeque<PendingSwitchEntry>`, one winner per harness, a failed attempt
  re-queued at the FRONT so a newer tap still wins). SCW-1/SCW-2, RLS-1 and the
  recovery-scan gating the fork carried are upstream's now (verified in the
  UPS-19 audit); the hard-cap rule above still applies to every new site.
- **Daemon surface**: status.json fork fields (`forecast`, `burn_aware`,
  `weekly_switch_threshold`, `last_error`), tokens.json feed, per-member
  gate/override socket commands, ccsbar/ccu client contracts.
- **Claude-side**: RESCUE-1
  dead-live-login reclaim, CLA-SPLIT hardening on top of merged #53
  (genuinely-long-lived engagement gate, force-snapshot guard), auth-broken
  quarantine surfaces, the `--new` login guard (lost in the UPS-17 merge,
  restored in the UPS-19 audit; `--codex` / `--browser` are upstream's now),
  and the daemon follow that never captures a refresh-less live login over its
  owner's chain.
  **NOT the Keychain write ordering any more** — the fork's "Keychain FIRST,
  then mutate" relink was dropped in UPS-17 for upstream's
  `publish_credential_link` + `keychain_mirror_source(Leave|SignOut)` shape,
  which rebuilt that path around a rename-not-unlink publish and an explicit
  absent-source policy. Same failure the fork's ordering was written for, now
  upstream's to keep correct.
  **NOT browser OAuth login itself** — upstream has that (`src/oauth_login.rs`).
- ~~**CLA-FEED session-token feed**~~ — **GONE from the fork delta (UPS-17,
  2026-09-09).** Contributed as PR #59, MERGED upstream as `rolling-token`
  (upstream commit `7340d44`, seven review rounds), and adopted back wholesale
  by this sync: the fork's `session_feed` flag, `feed_session_token`,
  `arm_feed_from_disk`, `feed_install_gate`, `claude_feed_tick`,
  `MINT_HORIZON_MS` and the `clauth feed` verb are DELETED in favour of
  upstream's `rolling_token` / `stamp_rolling_token` / `arm_rolling_from_disk`
  / `rolling_install_gate` / `restamp_rolling_token` / `sidecar_kind_of` and
  the `clauth rolling-token` + `clauth static-token [--clear]` verbs. The one
  line the fork still owns is a serde ALIAS: `#[serde(alias = "session_feed")]`
  on `ProfileConfig::rolling_token`, so a profile this fork armed under the old
  key stays armed across the upgrade (upstream deliberately carries no alias —
  no released upstream ever wrote that key). A rewrite normalizes it to
  `rolling_token`, and the unmodelled-key carry treats any alias as modelled
  (UPS-19: it used to carry the alias beside its field and brick the file).
  Design rationale kept at `docs/cla-feed/DESIGN.md`, marked superseded.
- **EXP-2 codex 401 kick**: upstream's now (`kick_codex` + `KICK_BREAKER`);
  it only works while the codex usage leg runs, which is why the proxy
  stand-down had to go.
- **Sessions/settings gating**: codex-harness profiles are invisible to
  upstream's settings sync and claude session machinery.
- **`clauth use-reset`** (2026-09-22): spends a codex account's banked
  usage-limit reset through codex's `wham/rate-limit-reset-credits`
  list/consume pair (`src/usage/codex_reset.rs`, `cmd_use_reset` in
  `main.rs`); the ccsbar account menu drives it. Fork-only; it sits on
  upstream's codex roster and store readers alone, so it is upstreamable as
  a standalone PR later.

## Contributing back

Contribution branches are cut from `upstream/mommy`, never from fork `main`
(`feat/scoped-weekly-walk` = PR #55 is the template): port the feature onto
upstream's shape, let the fork adopt the upstream form back on the next sync.
The fork's standing upstream threads live in `.agent/PROGRESS.md`.

**Endgame status (UPS-17, 2026-09-09):** the maintainer closed the design
thread on 2026-09-02 — "#69 supersedes it and #51 will not be merged" — and
left PR #69 in CHANGES_REQUESTED with ten required items, roughly fourteen
minors, and a head that no longer merges or compiles against `mommy`. So the
fork keeps its own codex engine (path A) until #69's round 2 lands: the two
codex layers now coexist by design, this fork's on `Profile`/`AppState`, and
upstream's future one on `codex-profiles.toml`. When #69 merges, the "Codex
engine" bullet collapses and a one-off migration moves the codex names out of
`profiles.toml` (`profiles`, `active_codex_profile`, `codex_fallback_chain`,
per-profile `harness`) into upstream's roster — neither upstream's `AppState`
nor #69's `CodexState::load` migrates them, and upstream's serde drops unknown
keys on the next save.

**The original plan (UPS-7, 2026-07-25):** upstream owns the codex
design now — `docs/codex-plan.md` on `mommy` is the spec, and we implement it
as a six-part series on a branch cut from the **v0.14 tag** (branch cut from a
TAG, not `upstream/mommy` — the one deliberate exception to the rule above).
It is a state-layer rewrite, not a port: harness moves out of
`Profile`/`AppState` into a separate `codex-profiles.toml`, dirs gain a `-cx`
suffix, and the store-mode refusal becomes a forced
`-c cli_auth_credentials_store="file"`. When that series lands, the "Codex
engine" bullet above collapses to whatever the fork still adds on top —
budget for the inventory to shrink, not grow.
