//! CDX-5: the opt-in localhost injection proxy (proxy-design.md). Codex points
//! at `http://127.0.0.1:<port>/backend-api/codex` via a printed
//! `[model_providers.clauth]` block; this proxy strips codex's own identity
//! headers, injects the selected pool account's `Authorization` +
//! `ChatGPT-Account-ID`, forwards to `https://chatgpt.com/backend-api/codex`,
//! and streams the SSE response back. On a pre-commit 429/401/5xx it rotates
//! to the next pool account and replays before the client sees a byte — the
//! true in-session fallback the whole CDX ladder points at.
//!
//! Deliberately its own process (`clauth proxy`), not the daemon: proxy-down
//! must be codex-down, never clauth-down, and a `pkill` daemon restart must
//! not sever in-flight codex streams. SSE-only, plain loopback HTTP, no TLS,
//! no WebSocket (proxy-design §1.2) — hand-rolled `TcpListener` + `ureq`
//! upstream, no new async runtime.

pub(crate) mod http;
pub(crate) mod pool;
pub(crate) mod sse;

use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::logline::logline;
use crate::profile_cache::{USAGE_CACHE_FILE, write_profile_cache};
use crate::usage::{UsageInfo, now_ms};

use self::http::{RequestError, RequestHead, read_body, read_request_head};
use self::pool::{Cooldowns, PoolMember, Selection, next_after_failure, select_account};
use crate::out::outln;

/// Default loopback port (unclaimed; overridable). Named in the printed config.
pub(crate) const DEFAULT_PROXY_PORT: u16 = 4517;

/// The ChatGPT-mode base the printed config points codex at, and the upstream
/// authority the proxy forwards to. Scheme+authority are constants — no client
/// byte can relocate them (proxy-design §1.4).
const UPSTREAM_BASE: &str = "https://chatgpt.com/backend-api/codex";

/// The path prefix codex requests under the printed provider config; a request
/// target not beginning here is answered 404 without forwarding.
const EXPECTED_PREFIX: &str = "/backend-api/codex";

/// Heartbeat file, touched on every proxied connection. `clauth doctor` reads
/// it to report whether the proxy has served recently. (It no longer stands
/// any usage leg down: the codex poll runs whole while the proxy serves.)
pub(crate) fn heartbeat_path() -> Result<PathBuf> {
    Ok(crate::profile::clauth_dir()?.join("codex-proxy.json"))
}

/// Print the `config.toml` block a user pastes to point codex at the proxy
/// (proxy-design §1.3 — clauth NEVER writes the live config).
pub(crate) fn print_config(port: u16) {
    outln!(
        "# Paste into ~/.codex/config.toml to route codex through clauth's proxy.\n\
         # (clauth never edits this file for you — proxy off = delete this block.)\n\
         model_provider = \"clauth\"\n\n\
         [model_providers.clauth]\n\
         name = \"openai\"\n\
         base_url = \"http://127.0.0.1:{port}{EXPECTED_PREFIX}\"\n\
         wire_api = \"responses\"\n\
         requires_openai_auth = true"
    );
}

/// Shared proxy state across connection threads.
struct ProxyState {
    cooldowns: Mutex<Cooldowns>,
    /// The upstream base URL requests are forwarded to. Production always uses
    /// [`UPSTREAM_BASE`]; the sandbox e2e points it at a local stub server.
    /// NEVER read from config (a client byte can never relocate it — §1.4).
    upstream_base: String,
}

/// Run the proxy until interrupted. Binds loopback only.
pub(crate) fn run(port: u16) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("failed to bind 127.0.0.1:{port} — is another proxy running?"))?;
    let state = std::sync::Arc::new(ProxyState {
        cooldowns: Mutex::new(Cooldowns::default()),
        upstream_base: UPSTREAM_BASE.to_string(),
    });

    outln!("clauth proxy listening on http://127.0.0.1:{port}{EXPECTED_PREFIX}");
    outln!("  point codex at it with:  clauth proxy --print-config --port {port}");
    outln!("  (loopback only, no client auth — any local process can use the pool)");
    // Stamp log lines like the daemon does: the proxy is a supervised process
    // whose stderr lands in `proxy.log`, and the 2026-07-18 incident had to be
    // reconstructed from an unstamped error flood.
    crate::logline::enable_timestamps();
    logline!("clauth proxy: listening on 127.0.0.1:{port}");
    touch_heartbeat(port);

    // Bounded thread-per-connection: codex holds few concurrent requests, so a
    // small cap can never throttle real use, but it stops a local slow-loris
    // (a trickle client resets the per-read timeout every byte) from exhausting
    // threads/FDs on clauth itself (review LOW). Over the cap → immediate 503.
    let live = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if live.load(std::sync::atomic::Ordering::Relaxed) >= MAX_CONCURRENT_CONNECTIONS {
                    let _ = http::write_error(&mut stream, "503 Service Unavailable", "proxy busy");
                    continue;
                }
                let state = std::sync::Arc::clone(&state);
                let live = std::sync::Arc::clone(&live);
                live.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                std::thread::spawn(move || {
                    touch_heartbeat(port);
                    if let Err(e) = handle_connection(&state, stream) {
                        logline!("clauth proxy: connection error: {e}");
                    }
                    live.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                });
            }
            Err(e) => logline!("clauth proxy: accept failed: {e}"),
        }
    }
    Ok(())
}

/// Concurrent-connection cap (review LOW: unbounded thread-per-connection).
/// Far above real codex concurrency; only a runaway/slow-loris ever hits it.
const MAX_CONCURRENT_CONNECTIONS: usize = 64;

/// Refresh the heartbeat file (atomic). Best-effort — a write failure just
/// means the passive leg may double-publish (benign, §1.7).
fn touch_heartbeat(port: u16) {
    let Ok(path) = heartbeat_path() else { return };
    let body = serde_json::json!({ "port": port, "at_ms": now_ms() }).to_string();
    let _ = crate::profile::atomic_write(&path, body.as_bytes());
}

/// Handle one client connection: read the request, then run the account-
/// rotating replay loop (proxy-design §2).
fn handle_connection(state: &ProxyState, stream: TcpStream) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let mut reader = BufReader::new(stream.try_clone().context("clone client stream")?);
    let mut client = stream;

    let head = match read_request_head(&mut reader) {
        Ok(h) => h,
        Err(RequestError::Malformed(m)) => {
            let _ = http::write_error(&mut client, "400 Bad Request", &m);
            return Ok(());
        }
        Err(e) => {
            let _ = http::write_error(&mut client, e.status(), "request rejected");
            return Ok(());
        }
    };

    // Fixed authority (§1.4): only our own prefix is forwarded; an absolute-
    // form target or a foreign path is a 404, never a forward.
    if !head.target.starts_with(EXPECTED_PREFIX) {
        return http::write_error(&mut client, "404 Not Found", "unknown path").map_err(Into::into);
    }
    // Codex issues POST /responses (the model turn) and GET /models (metadata
    // refresh) under our prefix — forward both with their own method. Any other
    // method is 405'd rather than silently rewritten (rewriting a non-idempotent
    // method was the original review LOW). Real-backend run 2026-07-16: 405'ing
    // GET /models made codex log errors and lose its model list, so it is now
    // forwarded too.
    let method_ok =
        head.method.eq_ignore_ascii_case("POST") || head.method.eq_ignore_ascii_case("GET");
    if !method_ok {
        return http::write_error(&mut client, "405 Method Not Allowed", "POST or GET only")
            .map_err(Into::into);
    }
    let body_len = match head.content_length() {
        Ok(n) => n,
        Err(e) => {
            let _ = http::write_error(&mut client, e.status(), "bad request framing");
            return Ok(());
        }
    };
    let body = read_body(&mut reader, body_len).context("read request body")?;

    forward_with_rotation(state, &head, &body, &mut client)
}

/// The replay loop (§2): pick an account, forward, and on a PRE-COMMIT
/// failure rotate to the next account and replay — up to one attempt per pool
/// member. Once any response byte has reached the client, propagate as-is.
fn forward_with_rotation(
    state: &ProxyState,
    head: &RequestHead,
    body: &[u8],
    client: &mut TcpStream,
) -> Result<()> {
    let ordered = pool_snapshot(state);
    if ordered.is_empty() {
        return http::write_error(
            client,
            "503 Service Unavailable",
            "no codex accounts in the pool",
        )
        .map_err(Into::into);
    }
    let active = active_codex(state);
    let now = now_ms();

    let mut current = match select_account(&ordered, active.as_deref(), now) {
        Selection::Use(name) => name,
        Selection::Exhausted => {
            return http::write_error(
                client,
                "429 Too Many Requests",
                "every codex account is in cooldown",
            )
            .map_err(Into::into);
        }
    };
    let mut tried: Vec<String> = Vec::new();

    loop {
        tried.push(current.clone());
        match forward_once(state, &current, head, body, client)? {
            ForwardOutcome::Streamed => return Ok(()),
            ForwardOutcome::UpstreamUnreachable => {
                return http::write_error(
                    client,
                    "502 Bad Gateway",
                    "codex upstream is unreachable",
                )
                .map_err(Into::into);
            }
            ForwardOutcome::PreCommitFailure { reset_ms } => {
                if let Ok(mut cd) = state.cooldowns.lock() {
                    cd.stamp(&current, now_ms(), reset_ms);
                }
                match next_after_failure(&ordered, &current, &tried, now_ms()) {
                    Selection::Use(next) => {
                        logline!("clauth proxy: rotating {current} → {next} (pre-commit failure)");
                        current = next;
                    }
                    Selection::Exhausted => {
                        return http::write_error(
                            client,
                            "429 Too Many Requests",
                            "every codex account rejected this request",
                        )
                        .map_err(Into::into);
                    }
                }
            }
        }
    }
}

enum ForwardOutcome {
    /// Response bytes reached the client (committed — never retried).
    Streamed,
    /// Upstream returned 429/401/5xx BEFORE any byte reached the client.
    PreCommitFailure { reset_ms: Option<u64> },
    /// A TRANSPORT error reaching the upstream (DNS/TLS/conn-refused). The
    /// upstream authority is a single fixed host, so this is not
    /// account-specific — walking the rest of the pool would re-hit the same
    /// unreachable host and pay the connect timeout per member (review LOW).
    /// Fail the request fast instead.
    UpstreamUnreachable,
}

/// Relay surviving inbound headers (minus the stripped set — §1.4), then inject
/// the identity + framing this proxy owns. Generic over ureq's request-body
/// typestate so one impl serves both the POST (/responses) and GET (/models)
/// builders.
fn inject_upstream_headers<B>(
    mut req: ureq::RequestBuilder<B>,
    head: &RequestHead,
    identity: &Identity,
) -> ureq::RequestBuilder<B> {
    for (name, value) in &head.headers {
        if !http::is_stripped_request_header(name) {
            req = req.header(name.as_str(), value.as_str());
        }
    }
    req.header("Host", "chatgpt.com")
        .header("Authorization", format!("Bearer {}", identity.access_token))
        .header("ChatGPT-Account-ID", identity.account_id.as_str())
        .header("Accept-Encoding", "identity")
}

/// One upstream attempt against `account`. Injects identity, forwards, and on
/// a < 400 status streams the body through to the client (capturing usage
/// headers). A 429/401/5xx returns [`ForwardOutcome::PreCommitFailure`]
/// WITHOUT writing anything to the client, so the caller can replay.
fn forward_once(
    state: &ProxyState,
    account: &str,
    head: &RequestHead,
    body: &[u8],
    client: &mut TcpStream,
) -> Result<ForwardOutcome> {
    let Some(identity) = account_identity(state, account) else {
        // Can't make this account usable (logged out / unrefreshable) — treat
        // as a pre-commit failure so the loop rotates past it.
        logline!("clauth proxy: '{account}' has no usable token — skipping");
        return Ok(ForwardOutcome::PreCommitFailure { reset_ms: None });
    };

    let url = format!(
        "{}{}",
        state.upstream_base,
        &head.target[EXPECTED_PREFIX.len()..]
    );
    // PROXY_AGENT carries http_status_as_error(false) so a 429/5xx returns
    // Ok(response) here, not Err — the pre-commit branch below reads status.
    // Forward with the request's OWN method: POST /responses carries the body;
    // GET /models is bodyless (handle_connection admits only these two).
    let send_result = if head.method.eq_ignore_ascii_case("GET") {
        inject_upstream_headers(crate::oauth::PROXY_AGENT.get(&url), head, &identity).call()
    } else {
        inject_upstream_headers(crate::oauth::PROXY_AGENT.post(&url), head, &identity).send(body)
    };
    let response = match send_result {
        Ok(r) => r,
        Err(e) => {
            logline!("clauth proxy: upstream transport error on '{account}': {e}");
            // The upstream host is fixed — a transport failure is not
            // account-specific, so fail fast rather than walk the pool.
            return Ok(ForwardOutcome::UpstreamUnreachable);
        }
    };
    let status = response.status().as_u16();
    // Capture usage from the flow-through rate-limit headers (§1.7) regardless
    // of status — even a 429 carries fresh counters.
    capture_usage_headers(account, &response);

    if status == 429 || status == 401 || status >= 500 {
        let reset_ms = parse_reset_header(&response);
        return Ok(ForwardOutcome::PreCommitFailure { reset_ms });
    }

    // Commit: write the status line + relayed headers, then stream the body.
    // Once a byte is written we are committed — every end shape below closes
    // this connection (truncated or complete), never replays (pre-commit rule).
    let started = Instant::now();
    let path = &head.target[EXPECTED_PREFIX.len()..];
    if let Err(e) = write_response_head(client, status, &response) {
        logline!(
            "clauth proxy: {account} {} {path} → {status} · client closed before head relay ({e})",
            head.method
        );
        return Ok(ForwardOutcome::Streamed);
    }
    let mut body_reader = response.into_body().into_reader();
    let mut sniffer = sse::TerminalSniffer::default();
    let mut relayed: u64 = 0;
    let mut buf = [0u8; 16 * 1024];
    let end = loop {
        let n = match body_reader.read(&mut buf) {
            Ok(0) => break RelayEnd::UpstreamEof,
            Ok(n) => n,
            Err(e) => break RelayEnd::UpstreamError(e),
        };
        // Sniff BEFORE write so the chunk carrying the terminal event is still
        // relayed, then the stream closes on our side — the upstream holds SSE
        // streams open past `response.completed` (see `proxy::sse`), so EOF
        // alone would leak this thread until the backstop timeout.
        let terminal = sniffer.feed(&buf[..n]);
        if let Err(e) = client.write_all(&buf[..n]) {
            break RelayEnd::ClientClosed(e);
        }
        relayed += n as u64;
        if terminal {
            break RelayEnd::Terminal;
        }
    };
    client.flush().ok();
    let secs = started.elapsed().as_secs();
    match end {
        // Normal ends — one summary line per request, stamped, greppable.
        RelayEnd::Terminal => {
            logline!(
                "clauth proxy: {account} {} {path} → {status} · {relayed}B in {secs}s · completed",
                head.method
            );
        }
        RelayEnd::UpstreamEof => {
            logline!(
                "clauth proxy: {account} {} {path} → {status} · {relayed}B in {secs}s · upstream EOF",
                head.method
            );
        }
        // The client bailed mid-relay (user interrupt, codex gave up) — its
        // decision, not a proxy fault; log as an end shape, not an error.
        RelayEnd::ClientClosed(e) => {
            logline!(
                "clauth proxy: {account} {} {path} → {status} · {relayed}B in {secs}s · client closed ({e})",
                head.method
            );
        }
        // The genuine anomaly: upstream died (or the backstop fired — the
        // elapsed seconds make that distinction readable) mid-stream, and the
        // client sees a truncated stream.
        RelayEnd::UpstreamError(e) => {
            logline!(
                "clauth proxy: {account} {} {path} → {status} · TRUNCATED after {relayed}B in {secs}s · upstream error: {e}",
                head.method
            );
        }
    }
    Ok(ForwardOutcome::Streamed)
}

/// How the committed relay of one response body ended.
enum RelayEnd {
    /// The sniffer saw a terminal SSE event (`response.completed` / `.failed`
    /// / `.incomplete` / `[DONE]`) — the turn is over; close without waiting
    /// for an upstream EOF that never comes.
    Terminal,
    /// Upstream finished the body (Content-Length'd responses, or a server
    /// that does close).
    UpstreamEof,
    /// The client hung up mid-relay.
    ClientClosed(std::io::Error),
    /// The upstream read failed mid-stream — the client sees truncation.
    UpstreamError(std::io::Error),
}

/// Write the committed response's status line + relayed headers (minus
/// hop-by-hop), forcing `Connection: close` (§1.8 single-request framing).
fn write_response_head(
    client: &mut TcpStream,
    status: u16,
    response: &ureq::http::Response<ureq::Body>,
) -> Result<()> {
    let reason = response.status().canonical_reason().unwrap_or("");
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in response.headers() {
        let lname = name.as_str().to_ascii_lowercase();
        if http::HOP_BY_HOP.contains(&lname.as_str()) {
            continue;
        }
        if let Ok(v) = value.to_str() {
            head.push_str(&format!("{}: {}\r\n", name.as_str(), v));
        }
    }
    head.push_str("Connection: close\r\n\r\n");
    client
        .write_all(head.as_bytes())
        .context("write response head")?;
    Ok(())
}

/// Identity for one account: the fresh access token + account id to inject.
struct Identity {
    access_token: String,
    account_id: String,
}

/// Resolve the injectable identity for `account` (proxy-design §1.6): its
/// store snapshot, refreshed through the standby leg when the access token is
/// inside the expiry margin. `None` when no usable token can be produced.
///
/// The fork's live-owner special case is gone with the engine that needed it.
/// `~/.codex/auth.json` is a SYMLINK onto the active profile's store now, so
/// reading the store IS reading the live file for that member — codex's own
/// refreshes land in the same bytes — and there is no second copy to prefer.
fn account_identity(state: &ProxyState, account: &str) -> Option<Identity> {
    ensure_fresh_parked(state, account);
    let auth = crate::codex_auth::read_store_auth(account)?;
    Some(Identity {
        access_token: auth.access_token()?.to_string(),
        account_id: auth.account_id()?.to_string(),
    })
}

/// Refresh a parked account's chain if it is due — delegating to the SHARED
/// single-writer entry point `codex_auth::standby_pass` (rotation guard,
/// in-guard re-read, no-replay memo, breaker). The proxy MUST NOT carry its
/// own refresh: the review-confirmed CRIT was exactly a second, guardless copy
/// here that read the token before the guard and double-spent the chain. There
/// is one implementation, and both the daemon's standby tick and this path go
/// through it — `standby_pass` decides due-ness itself, under the guard, which
/// is stricter than the pre-gate this used to do outside it.
fn ensure_fresh_parked(_state: &ProxyState, account: &str) {
    let outcome = crate::codex_auth::standby_pass(
        account,
        now_ms() as i64,
        chrono::Utc::now().to_rfc3339(),
        &crate::codex_auth::refresh_codex_chain,
    );
    if outcome == crate::codex_auth::StandbyOutcome::Failed {
        logline!("clauth proxy: parked refresh for '{account}' failed (will retry)");
    }
}

/// Parse usage from the upstream `x-codex-*` rate-limit headers and write the
/// account's usage cache (§1.7) — per-account live usage, zero extra requests.
fn capture_usage_headers(account: &str, response: &ureq::http::Response<ureq::Body>) {
    let h = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    // Read only the default `x-codex-*` header family. Model-specific quota
    // families use a longer prefix (for example
    // `x-codex-bengalfox-primary-*`) and are intentionally ignored.
    // A window from the header family, plus the minutes that say WHICH window
    // it is: codex names them `primary`/`secondary`, and which one is the 5h
    // and which the weekly is a property of its length, not of its name (a
    // weekly-only account publishes `primary` as its week).
    let window = |prefix: &str| -> Option<(crate::usage::UsageWindow, i64)> {
        let pct: f64 = h(&format!("x-codex-{prefix}-used-percent"))?.parse().ok()?;
        let resets_at = h(&format!("x-codex-{prefix}-reset-at"))
            .and_then(|s| s.parse::<i64>().ok())
            .map(crate::usage::epoch_secs_to_iso);
        let minutes = h(&format!("x-codex-{prefix}-window-minutes"))
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        Some((
            crate::usage::UsageWindow {
                utilization: pct,
                resets_at,
            },
            minutes,
        ))
    };
    let primary = window("primary");
    let secondary = window("secondary");
    if primary.is_none() && secondary.is_none() {
        return; // no rate-limit headers on this response
    }
    // Anything at or past a day is the weekly slot; everything shorter is the
    // 5h one. The same rule upstream's body mapper applies to the JSON shape.
    const DAY_MINUTES: i64 = 24 * 60;
    let mut five_hour = None;
    let mut seven_day = None;
    for (w, minutes) in [primary, secondary].into_iter().flatten() {
        if minutes >= DAY_MINUTES {
            seven_day = Some(w);
        } else {
            five_hour = Some(w);
        }
    }
    let info = UsageInfo {
        five_hour,
        seven_day,
        codex_limit_reached: h("x-codex-rate-limit-reached-type").filter(|s| !s.is_empty()),
        ..UsageInfo::default()
    };
    write_profile_cache(
        &crate::profile::ProfileName::from(account),
        USAGE_CACHE_FILE,
        &info,
    );
}

/// The advertised reset (epoch-ms) from the primary window's reset header, for
/// the cooldown stamp.
fn parse_reset_header(response: &ureq::http::Response<ureq::Body>) -> Option<u64> {
    response
        .headers()
        .get("x-codex-primary-reset-at")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok())
        .and_then(|secs| u64::try_from(secs).ok())
        .map(|secs| secs * 1000)
}

/// The pool in chain order (proxy-design §1.5): `codex_fallback_chain` when
/// non-empty, else every codex profile with a stored login. Availability =
/// not auth_broken, not leased, not in cooldown; cached-usage exhaustion is
/// carried separately as ADVISORY rank (see [`PoolMember::cached_spent`]) —
/// it deprioritizes a member but never excludes it, so a stale cache can't
/// wedge the proxy into 429ing traffic upstream would have served.
fn pool_snapshot(state: &ProxyState) -> Vec<PoolMember> {
    let Ok(codex) = crate::codex_profiles::CodexState::load() else {
        return Vec::new();
    };
    // The chain when there is one, else the whole roster — the pool is every
    // account the operator has told clauth about, in the order they set.
    let names: Vec<crate::profile::ProfileName> = if codex.fallback_chain().is_empty() {
        codex.profiles().to_vec()
    } else {
        codex.fallback_chain().to_vec()
    };
    let cooldowns = state.cooldowns.lock().ok();
    let now = now_ms();
    // The codex chain's own weekly line (codex-profiles.toml), not the claude
    // one: the two chains have had independent lines since the file split.
    let weekly_pct = codex.weekly_switch_threshold_pct();
    names
        .into_iter()
        .filter(|n| crate::codex_auth::read_store_auth(n.as_str()).is_some())
        .map(|name| {
            let cooldown_until_ms = cooldowns
                .as_ref()
                .map(|c| c.get(name.as_str()))
                .unwrap_or(0);
            // Quarantined = the server declared this chain dead; a live codex
            // session owns its account until it exits.
            let unavailable = crate::codex_auth::read_quarantine(name.as_str()).is_some()
                || crate::runtime::has_live_session(&name);
            let cached_spent = cached_exhausted(&name, now, weekly_pct);
            PoolMember {
                name: name.to_string(),
                cooldown_until_ms,
                unavailable,
                cached_spent,
            }
        })
        .collect()
}

/// Whether `name`'s cached usage says it is spent (the CDX-4 exhaustion shape
/// against its own cache). Best-effort — no cache = not exhausted.
fn cached_exhausted(name: &crate::profile::ProfileName, now_ms: u64, weekly_pct: f64) -> bool {
    let now_secs = (now_ms / 1000) as i64;
    let Some(info) = crate::profile_cache::load_profile_cache::<UsageInfo>(name, USAGE_CACHE_FILE)
    else {
        return false;
    };
    // A window still counts only while its own reset is in the future: a lapsed
    // `resets_at` means the cache is describing a window that has already
    // rolled over, and treating that as spent would park a healthy account.
    let live = |w: &crate::usage::UsageWindow, line: f64| {
        w.utilization >= line
            && w.resets_at
                .as_deref()
                .and_then(crate::usage::iso_to_epoch_secs)
                .is_none_or(|at| at > now_secs)
    };
    info.five_hour.as_ref().is_some_and(|w| live(w, 100.0))
        || info.seven_day.as_ref().is_some_and(|w| live(w, weekly_pct))
        || info.codex_limit_reached.is_some()
}

fn active_codex(_state: &ProxyState) -> Option<String> {
    crate::codex_profiles::CodexState::load()
        .ok()?
        .active_profile()
        .map(|n| n.as_str().to_string())
}

#[cfg(test)]
impl ProxyState {
    /// Build a state pointed at a stub upstream — the e2e seam (§1.4: the
    /// base is never config-derived in production).
    fn for_test(upstream_base: String) -> Self {
        Self {
            cooldowns: Mutex::new(Cooldowns::default()),
            upstream_base,
        }
    }
}

/// Write a heartbeat under the sandbox home — for doctor/passive-leg tests.
#[cfg(test)]
pub(crate) fn touch_heartbeat_for_test(port: u16) {
    touch_heartbeat(port);
}

/// Accept and handle exactly one connection — the e2e driver (production uses
/// the `incoming()` loop in [`run`]).
#[cfg(test)]
pub(crate) fn serve_one_for_test(upstream_base: String, listener: &TcpListener) -> Result<()> {
    let state = ProxyState::for_test(upstream_base);
    let (stream, _) = listener.accept().context("accept")?;
    handle_connection(&state, stream)
}

#[cfg(test)]
#[path = "../../tests/inline/proxy.rs"]
mod tests;
