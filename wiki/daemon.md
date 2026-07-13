# `clauth daemon` — headless scheduler + status feed

`clauth daemon` runs the same background refresher the TUI runs
(`spawn_refresher`) with no UI: refresh usage, rotate expiring tokens, execute
the fallback chain's auto-switches, and publish `~/.clauth/status.json` for
external readers. `clauth status --json` prints the same shape single-shot,
no daemon required. This document is the read contract for both.

Scope note: the daemon carries **no external mutation surface** — no socket,
no config endpoint. Anything that needs eyes (a diverged live login, a manual
Claude Code `/login`, an unprovable identity) is refused and logged; the TUI
stays the only resolution surface.

## Process model

- **Singleton**: one advisory lock (`~/.clauth/clauthd.lock`) held for the
  process lifetime. A second `clauth daemon` blocks in standby and takes over
  the moment the holder exits — a dead holder's flock auto-releases, so a
  supervisor (`launchd`/`systemd`) with restart-on-crash keeps exactly one
  scheduler alive without pidfile bookkeeping.
- **Watchdog**: the state flock has no deadline and a switch shells out inside
  it, so a wedged tick can freeze the single-threaded loop. If no tick
  completes within 60 s the daemon `abort()`s for a clean supervisor restart.
- **Log hygiene**: every daemon-visible stderr line carries an ISO-8601 UTC
  prefix (enabled only in daemon mode — interactive stderr stays bare), and
  `~/.clauth/daemon.log` is size-capped in place when a supervisor points
  stderr at it. The in-place trim is only sound for an APPEND-mode fd: use
  launchd `StandardErrorPath` or systemd `StandardError=append:...` — a
  non-append redirect (`file:`, a plain `>`) keeps its own offset, so the
  next write after a trim leaves a sparse NUL hole and the size cap is
  defeated.
- **Dual-scheduler dedup (TUI stand-down)**: a TUI opened alongside the daemon
  probes the singleton lock + feed freshness every tick and stands its own
  refresher down while the daemon is live — rendering from the daemon's disk
  caches instead of double-polling the usage API, double-rotating the
  single-use refresh chain, or re-deciding its switches. It re-arms within one
  tick of the daemon dying (lock released) or wedging (feed stale).

## `~/.clauth/status.json`

Written each scheduler tick and immediately after a switch lands. Atomic
(`tmp` + rename into place), `0600`. **Never carries a token, secret, or
key** — names, tiers, percentages, timestamps only.

```json
{
  "schema": 1,
  "generated_at": "2026-07-03T19:04:40+00:00",
  "active_profile": "kitty",
  "pending_switch": null,
  "wrap_off": false,
  "refresh_interval_ms": 300000,
  "profiles": [
    {
      "name": "kitty",
      "active": true,
      "provider": "anthropic",
      "base_url": null,
      "tier": "Max 5x",
      "has_live_session": true,
      "auth_status": "ok",
      "fetch_status": "Fresh",
      "stale": false,
      "fetched_at": "2026-07-03T19:04:20+00:00",
      "next_refresh_at": "2026-07-03T19:09:20+00:00",
      "auto_start": true,
      "bell_threshold": 90,
      "fallback": { "position": 1, "threshold": 95.0, "armed": true },
      "windows": [
        { "label": "5h",      "utilization_pct": 42.0, "resets_at": "2026-07-03T23:00:00+00:00" },
        { "label": "7d",      "utilization_pct": 18.0, "resets_at": "2026-07-08T17:00:00+00:00" },
        { "label": "7d Opus", "utilization_pct": 30.0, "resets_at": "2026-07-08T17:00:00+00:00" }
      ],
      "third_party": null
    }
  ]
}
```

### Field notes

| Field | Semantics |
|---|---|
| `schema` | Integer, currently `1`. Bumped ONLY on a breaking change; additive fields do not bump it (evolution rule below). |
| `generated_at` | Write stamp, ISO-8601 UTC with an explicit `+00:00` offset (all timestamps are; parse the offset, never key on a `Z` suffix — the writer does not emit one). Readers derive staleness from it: a stamp much older than `refresh_interval_ms` means the daemon is gone/stuck — show last-known data with a stale cue, never spin. |
| `active_profile` | The profile whose credentials are currently installed, else `null`. |
| `pending_switch` | A switch the daemon has accepted but not yet applied (`"<name>"`), else `null`. Exists so readers can show in-flight truth instead of a timing heuristic. Always `null` from the single-shot CLI. |
| `wrap_off` | The fallback chain's stop-vs-stay-on-last flag, verbatim from state. |
| `profiles[].provider` | `"anthropic"` for OAuth profiles, else the recognised provider's display name. |
| `profiles[].tier` | Plan label (`"Max 5x"`…), `null` when unknown. Opaque display string. |
| `profiles[].auth_status` | `"ok" \| "expiring" \| "broken"`. `broken` = last refresh rejected as revoked/invalid → excluded from fallback walks, refused as a switch target. `expiring` = past expiry, not yet refreshed. `broken` outranks `expiring`. Absent ⇒ `"ok"`. |
| `profiles[].fetch_status` | `"Fresh" \| "Cached" \| "Failed" \| "RateLimited"` — the usage fetch's last outcome, so readers can distinguish live bars from last-known. `Failed`/`RateLimited` come only from a live daemon's OAuth fetch leg; api-key profiles (and any name the live stores don't carry yet) derive `Fresh`/`Cached` from their own cache's mtime instead — an api-key profile with a warm cache is never reported as unfetched. `null` = no cache at all (genuinely never fetched). |
| `profiles[].stale` | `bool` (additive, schema stays 1; absent ⇒ `false`). `true` when the daemon distrusts this reading as a **deep-slot stuck `RateLimited`**: `fetch_status == "RateLimited"` AND the consecutive-429 streak has passed the active-retry cap, so the `/usage` throttle never drained and no `Fresh` read is coming. This is the same judgment the daemon's auto-switch acts on — a stuck-RateLimited active bypasses the "only act on a Fresh read" gate so the chain rotates away instead of wedging (the `RateLimited` analogue of the `auth_status: "broken"` bypass); the switch still requires the active's last-known usage to be genuinely spent, so a throttle blip with headroom stays put. Readers should dim the meter / show a "stuck" cue rather than render the frozen number as current truth. `false` for a shallow/transient `RateLimited`, for every non-`RateLimited` status, and **always** for the single-shot `clauth status --json` (no daemon, no streak history). |
| `profiles[].fallback` | `null` when not in the chain; else 1-based `position`, `threshold` (%), `armed` (this member is the active one the auto-switch watches). |
| `profiles[].windows[]` | `label` is **derived, not an enum**: `"5h"` and `"7d"` always; the third is a plan-tier label (`"7d Opus"`…). Treat labels as opaque display strings, never keys to switch on. `utilization_pct` 0–100 float; `resets_at` nullable. |
| `profiles[].third_party` | `{ "available": bool }` for api-key profiles once probed, else `null` — including an api-key profile whose provider has never been reached (no cache yet). Plain reachability; structured balances deliberately deferred. |

### Evolution rule (the load-bearing part)

- **Writers**: additive only under the same `schema` — new fields may appear,
  existing fields never change type/meaning. A breaking change bumps `schema`.
- **Readers**: ignore unknown fields; default absent optional fields (absent
  `auth_status` ⇒ `"ok"`); refuse only on `schema` greater than what they
  know, showing "daemon newer than me" rather than garbage.

## `clauth status --json`

Same schema, produced single-shot from the on-disk caches with no daemon and
no network fetch: `pending_switch` is always `null`, `generated_at` is the
print stamp, and freshness/next-refresh derive from each profile's usage-cache
mtime. One code path builds both (`daemon::status_json::build_status`), so the
key SHAPE cannot drift between the daemon feed and the CLI snapshot. Values can:
the single-shot form derives `fetch_status` from cache mtimes, so it only ever
reports `Fresh`/`Cached`/`null` — a profile the live daemon shows as `Failed` or
`RateLimited` reads as `Cached` here at the same instant. Poll the feed, not the
CLI, when the fetch outcome matters.
