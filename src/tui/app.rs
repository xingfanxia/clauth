//! Application state, keymap, and tick logic.
//!
//! Layout invariants:
//!   - The main menu is a single list with profile rows followed by a small
//!     set of action rows. Indices into this list live in `main_cursor`.
//!   - The chain editor is a second screen with its own cursor.
//!   - Modals stack: the top of `modals` owns input; events fall through to
//!     the screen below only when the stack is empty.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::actions::{
    CaptureSnapshot, capture_into_profile, capture_snapshot, create_blank_profile, delete_profile,
    edit_profile_endpoint, find_matching_oauth_profile, rename_profile, reorder_profile,
    switch_profile, validate_profile_name,
};
use crate::claude::{
    LinkState, adopt_first_login, classify_credentials_link, credentials_diverged,
    detach_credentials_link, force_link_profile_credentials, force_snapshot_active_credentials,
    is_first_login, link_profile_credentials, read_claude_credentials, snapshot_active_credentials,
};
use crate::fallback::{DEFAULT_THRESHOLD, auto_switch_if_needed, threshold_for};
use crate::lock::with_state_lock;
use crate::oauth;
use crate::profile::{
    AppConfig, Profile, app_state_mtime, load_config, save_app_state, save_profile,
};
use crate::usage::{
    ActivityKind, ActivityStore, ConsecutiveCacheHit, ConsecutiveOk, Last429At, LastFetchedAt,
    LastRotatedWindow, LearnedIntervals, NextRefreshPerProfile, OpResult, OpResultReceiver,
    OpResultSender, PendingAutoStart, PendingSwitch, PendingWindowRotation, ProfileActivity,
    RefetchQueue, SERVER_CACHE_TTL_ESTIMATE_MS, StartupReceiver, StartupSender, StartupSignal,
    StatusStore, TokenEntry, TokenList, UsageStore, any_busy, clear_activity,
    default_fallback_threshold, fetch_all_into, is_idle, mark_activity, spawn_refresher,
};

// ── Shared input field ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct InputState {
    pub(crate) value: String,
    /// Caret position in UTF-8 byte offset. Always falls on a char boundary.
    pub(crate) cursor: usize,
}

impl InputState {
    pub(crate) fn new(initial: &str) -> Self {
        Self {
            value: initial.to_string(),
            cursor: initial.len(),
        }
    }

    pub(crate) fn insert(&mut self, ch: char) {
        self.value.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    pub(crate) fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.value[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.value.replace_range(prev..self.cursor, "");
        self.cursor = prev;
    }

    pub(crate) fn delete(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        let next = self.value[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.cursor + i)
            .unwrap_or(self.value.len());
        self.value.replace_range(self.cursor..next, "");
    }

    pub(crate) fn left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = self.value[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
    }

    pub(crate) fn right(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        self.cursor = self.value[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.cursor + i)
            .unwrap_or(self.value.len());
    }

    pub(crate) fn home(&mut self) {
        self.cursor = 0;
    }

    pub(crate) fn end(&mut self) {
        self.cursor = self.value.len();
    }

    pub(crate) fn trimmed(&self) -> &str {
        self.value.trim()
    }

    pub(crate) fn trimmed_some(&self) -> Option<String> {
        let t = self.trimmed();
        (!t.is_empty()).then(|| t.to_string())
    }
}

// ── Modals ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub(crate) enum ChainAction {
    Threshold,
    MoveUp,
    MoveDown,
    Remove,
    Back,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EndpointField {
    BaseUrl,
    ApiKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NewProfileField {
    Name,
    BaseUrl,
    ApiKey,
}

#[derive(Debug, Clone)]
pub(crate) struct NewProfileForm {
    pub(crate) name: InputState,
    pub(crate) base_url: InputState,
    pub(crate) api_key: InputState,
    pub(crate) focus: NewProfileField,
}

#[derive(Debug, Clone)]
pub(crate) struct EditProfileForm {
    pub(crate) name: String,
    pub(crate) base_url: InputState,
    pub(crate) api_key: InputState,
    pub(crate) focus: EndpointField,
}

#[derive(Debug, Clone)]
pub(crate) struct RenameForm {
    pub(crate) old: String,
    pub(crate) input: InputState,
}

#[derive(Debug, Clone)]
pub(crate) struct ConfirmState {
    pub(crate) message: String,
    pub(crate) detail: Option<String>,
    /// Currently highlighted choice; false = no/cancel, true = yes/confirm.
    pub(crate) choice: bool,
    pub(crate) on_confirm: ConfirmAction,
}

#[derive(Debug, Clone)]
pub(crate) enum ConfirmAction {
    Delete(String),
    /// `bool` = `from_divergence`, carried through so the conflict path
    /// keeps the deferred-detach semantics of the `NewProfile` divergence flow.
    CaptureConflict(Box<CaptureSnapshot>, bool),
    Switch(String),
    /// Confirm step before discarding CC's freshly-written credentials and
    /// re-linking the live path to the named profile's stored creds.
    DiscardDivergence(String),
    /// Force-rotate every profile's refresh token, bypassing the live-session
    /// guard. Profiles with an active `clauth start` session may be logged out.
    RotateAll,
}

#[derive(Debug, Clone)]
pub(crate) struct CaptureNameForm {
    pub(crate) snapshot: Box<CaptureSnapshot>,
    pub(crate) input: InputState,
    /// Set when the capture was initiated by the `NewProfile` divergence
    /// choice. Deferring the destructive detach + deactivate of the
    /// previously-active profile to the capture's success arm keeps that
    /// profile linked and active if the user cancels the name modal.
    pub(crate) from_divergence: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DivergenceChoice {
    Overwrite,
    NewProfile,
    Discard,
}

/// Modal state for the credential-divergence prompt. Shown both at startup
/// and whenever the 1Hz runtime poll detects the live .credentials.json no
/// longer resolves to the active profile's stored creds. Three explicit
/// actions: take CC's new creds into the active profile, save them as a
/// new profile, or discard them and restore the profile's stored identity.
#[derive(Debug, Clone)]
pub(crate) struct DivergenceForm {
    pub(crate) active: String,
    pub(crate) cursor: usize,
}

impl DivergenceForm {
    pub(crate) fn options() -> [DivergenceChoice; 3] {
        [
            DivergenceChoice::Overwrite,
            DivergenceChoice::NewProfile,
            DivergenceChoice::Discard,
        ]
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ChainItemMenuState {
    pub(crate) name: String,
    pub(crate) cursor: usize,
}

/// Per-profile actions popup. Cursor is into the list returned by
/// `profile_menu_options` so disabled / context-sensitive entries shift
/// without the caller having to remember which row is which.
#[derive(Debug, Clone)]
pub(crate) struct ProfileMenuState {
    pub(crate) name: String,
    pub(crate) cursor: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProfileMenuAction {
    Edit,
    Rename,
    ToggleAutoStart,
    Delete,
    Back,
}

#[derive(Debug, Clone)]
pub(crate) struct ChainAddState {
    pub(crate) candidates: Vec<String>,
    pub(crate) cursor: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct ChainThresholdForm {
    pub(crate) name: String,
    pub(crate) input: InputState,
}

#[derive(Debug, Clone)]
pub(crate) enum Modal {
    NewProfile(NewProfileForm),
    EditProfile(EditProfileForm),
    Rename(RenameForm),
    Confirm(ConfirmState),
    /// Credential divergence prompt — Overwrite / NewProfile / Discard.
    Divergence(DivergenceForm),
    CaptureName(CaptureNameForm),
    ProfileMenu(ProfileMenuState),
    ChainItemMenu(ChainItemMenuState),
    ChainAdd(ChainAddState),
    ChainThreshold(ChainThresholdForm),
    Help,
}

// ── Toasts ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToastKind {
    Info,
    Success,
    Warning,
    Danger,
}

#[derive(Debug, Clone)]
pub(crate) struct Toast {
    pub(crate) kind: ToastKind,
    pub(crate) body: String,
    pub(crate) born: Instant,
}

/// Confirm modal copy for the force-rotate-all action.
const ROTATE_ALL_MSG: &str = "rotate tokens for all accounts?";
const ROTATE_ALL_DETAIL: &str = "accounts with a live session might be logged out.";

/// Maximum on-screen toasts at any one time; older expire to make room.
const TOAST_CAPACITY: usize = 4;
/// How long a toast stays visible before fading off the stack.
const TOAST_TTL: Duration = Duration::from_secs(4);

// ── Screens ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Screen {
    Overview,
    Chain,
    ProfileDetail { profile_index: usize },
}

// ── Overview list items ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub(crate) enum MainItemKind {
    Profile(usize),
    /// Visual break between the profile rows and action rows. Cursor steps
    /// over it and Enter is a no-op.
    ActionSeparator,
    NewProfile,
    CaptureCredentials,
    OpenChain,
}

// ── App ───────────────────────────────────────────────────────────────────────

pub(crate) struct App {
    /// Shared mutable state — locked by the main thread on every read/write
    /// and by the background usage refresher when rotating tokens or kicking
    /// auto-start. Hold the guard only across the work that needs it; releasing
    /// before HTTP-heavy operations keeps the refresher from stalling the UI.
    pub(crate) config: Arc<Mutex<AppConfig>>,

    pub(crate) usage_store: UsageStore,
    pub(crate) usage_status: StatusStore,
    pub(crate) usage_tokens: TokenList,
    pub(crate) next_refresh_per_profile: NextRefreshPerProfile,
    /// One source of truth for "what's happening to profile X right now",
    /// shared between the scheduler thread, oauth refresh paths, and the TUI
    /// render loop. The render loop reads it on every frame to drive the
    /// spinner; workers write per-name. Never held across HTTP.
    pub(crate) activity: ActivityStore,
    /// Drained inside `on_tick`. Each result clears its profile's
    /// `ActivityStore` slot and surfaces any error as a danger toast.
    pub(crate) op_results: OpResultReceiver,
    /// Sender side. Cloned into workers (and passed to refresh/rotation
    /// helpers) so they can report completion without holding any lock.
    pub(crate) op_sender: OpResultSender,
    /// Startup phase signals from the reconcile / bootstrap background workers.
    /// Drained inside `on_tick`; sequences the event loop through reconcile →
    /// bootstrap without blocking the first paint on HTTP or an FS walk.
    pub(crate) startup_results: StartupReceiver,
    /// Sender side, cloned into the two startup workers.
    pub(crate) startup_sender: StartupSender,
    pub(crate) last_fetched: LastFetchedAt,
    pub(crate) pending_auto_start: PendingAutoStart,
    pub(crate) pending_window_rotation: PendingWindowRotation,
    pub(crate) last_rotated_window: LastRotatedWindow,
    /// Scheduler-posted auto-switch decisions. Drained inside `on_tick` and
    /// dispatched to the same switch worker pipeline as user-initiated
    /// switches.
    pub(crate) pending_switch: PendingSwitch,
    pub(crate) refetch_queue: RefetchQueue,
    pub(crate) learned_intervals: LearnedIntervals,
    pub(crate) ok_count: ConsecutiveOk,
    pub(crate) cache_hit_count: ConsecutiveCacheHit,
    pub(crate) last_429: Last429At,

    pub(crate) screen: Screen,
    pub(crate) modals: Vec<Modal>,

    pub(crate) main_cursor: usize,
    pub(crate) chain_cursor: usize,

    pub(crate) toasts: VecDeque<Toast>,

    pub(crate) last_state_mtime: Option<SystemTime>,
    pub(crate) started_at: Instant,
    /// Monotonically increasing render-tick counter used to advance the
    /// activity spinner frame. Bumped once per `on_tick`.
    pub(crate) tick_count: u64,
    pub(crate) quit: bool,
    /// Last time the 1Hz divergence poll ran. Re-checks whether
    /// `~/.claude/.credentials.json` still points at the active profile and
    /// pushes a Divergence modal when CC has overwritten the symlink
    /// (typically by `/login`). Defers behind any open modal.
    pub(crate) last_divergence_check: Instant,
    /// True once the startup reconcile worker has reported back. Gates the
    /// bootstrap spawn so token rotation never races a soon-to-be-disowned
    /// profile (the reconcile prompt must be resolved first).
    pub(crate) reconcile_done: bool,
    /// True once `spawn_bootstrap` has been dispatched. Guards against a
    /// second dispatch on subsequent ticks.
    pub(crate) bootstrap_started: bool,
    /// True while the bootstrap worker is still running (set before spawn,
    /// cleared inside the worker on every exit path). `ConfirmAction::RotateAll`
    /// consults this alongside `any_busy` to block a concurrent rotate-all
    /// from racing the bootstrap's `auto_start_windows` leg.
    pub(crate) bootstrap_active: Arc<AtomicBool>,
}

impl App {
    pub(crate) fn new(config: AppConfig) -> Self {
        let usage_store: UsageStore = Arc::new(Mutex::new(HashMap::new()));
        let usage_status: StatusStore = Arc::new(Mutex::new(HashMap::new()));
        let usage_tokens: TokenList = Arc::new(Mutex::new(collect_tokens(&config.profiles)));
        let next_refresh_per_profile: NextRefreshPerProfile = Arc::new(Mutex::new(HashMap::new()));
        let activity: ActivityStore = Arc::new(Mutex::new(HashMap::new()));
        let (op_sender, op_results) = std::sync::mpsc::channel::<OpResult>();
        let (startup_sender, startup_results) = std::sync::mpsc::channel::<StartupSignal>();
        let last_fetched: LastFetchedAt = Arc::new(Mutex::new(HashMap::new()));
        let pending_auto_start: PendingAutoStart = Arc::new(Mutex::new(HashSet::new()));
        let pending_window_rotation: PendingWindowRotation = Arc::new(Mutex::new(HashMap::new()));
        let last_rotated_window: LastRotatedWindow = Arc::new(Mutex::new(HashMap::new()));
        let pending_switch: PendingSwitch = Arc::new(Mutex::new(HashSet::new()));
        let refetch_queue: RefetchQueue = Arc::new(Mutex::new(HashSet::new()));
        // Restore AIMD state from disk so cadence survives restarts.
        let learned_intervals: LearnedIntervals =
            Arc::new(Mutex::new(config.state.learned_intervals_ms.clone()));
        let ok_count: ConsecutiveOk =
            Arc::new(Mutex::new(config.state.consecutive_ok_count.clone()));
        // Restore cache-hit counters only when learned < TTL — above TTL the
        // counter is irrelevant (polling is slow enough that cache hits can't
        // occur in steady state), and below TTL it captures mid-elimination
        // state that would otherwise need another hit-pair to correct.
        let restored_ch: HashMap<String, u32> = config
            .state
            .consecutive_cache_hit_count
            .iter()
            .filter(|(name, _)| {
                config
                    .state
                    .learned_intervals_ms
                    .get(*name)
                    .copied()
                    .is_some_and(|l| l < SERVER_CACHE_TTL_ESTIMATE_MS)
            })
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        let cache_hit_count: ConsecutiveCacheHit = Arc::new(Mutex::new(restored_ch));
        let last_429: Last429At = Arc::new(Mutex::new(config.state.last_429_at.clone()));

        Self {
            config: Arc::new(Mutex::new(config)),
            usage_store,
            usage_status,
            usage_tokens,
            next_refresh_per_profile,
            activity,
            op_results,
            op_sender,
            startup_results,
            startup_sender,
            last_fetched,
            pending_auto_start,
            pending_window_rotation,
            last_rotated_window,
            pending_switch,
            refetch_queue,
            learned_intervals,
            ok_count,
            cache_hit_count,
            last_429,
            screen: Screen::Overview,
            modals: Vec::new(),
            main_cursor: 0,
            chain_cursor: 0,
            toasts: VecDeque::new(),
            last_state_mtime: app_state_mtime(),
            started_at: Instant::now(),
            tick_count: 0,
            quit: false,
            last_divergence_check: Instant::now(),
            reconcile_done: false,
            bootstrap_started: false,
            bootstrap_active: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Lock the shared AppConfig. Holds the lock for the lifetime of the
    /// returned guard. Order: AppConfig mutex outer, `with_state_lock` inner;
    /// the inner is taken by the actions that mutate disk state.
    pub(crate) fn config(&self) -> MutexGuard<'_, AppConfig> {
        self.config.lock().expect("config mutex poisoned")
    }

    /// Spawn the startup bootstrap onto a background thread so it never blocks
    /// the first paint. The worker re-links the active profile's credentials,
    /// rotates every profile's OAuth pair (`refresh_all`), runs the initial
    /// usage fetch, and kicks any opted-in 5h windows (`auto_start_windows`).
    /// All of this is HTTP-heavy and used to run on the UI thread before the
    /// event loop ever painted.
    ///
    /// Per-profile spinners light up from inside `refresh_all` / `fetch_all_into`
    /// / `auto_start_windows` (they mark `Refreshing` / `Fetching` /
    /// `AutoStarting`), and per-profile completion toasts ride the existing
    /// `OpResult` drain. When the worker finishes it posts
    /// `StartupSignal::BootstrapDone`; the UI thread then rebuilds the token
    /// snapshot, spawns the scheduler, applies usage, and runs the one-shot
    /// startup auto-switch — all fast, lock-scoped, network-free work.
    pub(crate) fn spawn_bootstrap(&self) {
        let config = Arc::clone(&self.config);
        let usage_store = Arc::clone(&self.usage_store);
        let usage_status = Arc::clone(&self.usage_status);
        let last_fetched = Arc::clone(&self.last_fetched);
        let refetch_queue = Arc::clone(&self.refetch_queue);
        let activity = Arc::clone(&self.activity);
        let learned_intervals = Arc::clone(&self.learned_intervals);
        let ok_count = Arc::clone(&self.ok_count);
        let cache_hit_count = Arc::clone(&self.cache_hit_count);
        let last_429 = Arc::clone(&self.last_429);
        let op_sender = self.op_sender.clone();
        let startup_sender = self.startup_sender.clone();
        let bootstrap_active = Arc::clone(&self.bootstrap_active);

        let startup_sender_for_panic = startup_sender.clone();
        let bootstrap_active_for_panic = Arc::clone(&bootstrap_active);
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Re-establish the credentials symlink that the previous shutdown
                // replaced with a plain file. Without this, in-session Claude Code
                // refreshes write to a standalone file instead of the profile.
                let active = config
                    .lock()
                    .expect("config mutex poisoned")
                    .state
                    .active_profile
                    .clone();
                if let Some(active) = active {
                    let _ = link_profile_credentials(&active);
                }

                // Refresh every profile's OAuth token pair — Claude Code does the
                // same thing silently on launch. Rotates and persists the new pair
                // so the initial usage fetch below uses fresh access tokens.
                let _ = oauth::refresh_all(&config, false, &refetch_queue, &activity, &op_sender);

                let snapshot =
                    collect_tokens(&config.lock().expect("config mutex poisoned").profiles);
                fetch_all_into(
                    &config,
                    &snapshot,
                    &usage_store,
                    &usage_status,
                    &last_fetched,
                    &refetch_queue,
                    &activity,
                    &learned_intervals,
                    &ok_count,
                    &cache_hit_count,
                    &last_429,
                );

                let started = oauth::auto_start_windows(
                    &config,
                    &usage_store,
                    &refetch_queue,
                    &activity,
                    &op_sender,
                );
                if !started.is_empty() {
                    let retry: Vec<TokenEntry> =
                        collect_tokens(&config.lock().expect("config mutex poisoned").profiles)
                            .into_iter()
                            .filter(|e| started.contains(&e.name))
                            .collect();
                    fetch_all_into(
                        &config,
                        &retry,
                        &usage_store,
                        &usage_status,
                        &last_fetched,
                        &refetch_queue,
                        &activity,
                        &learned_intervals,
                        &ok_count,
                        &cache_hit_count,
                        &last_429,
                    );
                }

                bootstrap_active.store(false, Ordering::SeqCst);
                let _ = startup_sender.send(StartupSignal::BootstrapDone);
            }));
            if result.is_err() {
                // Panic path: clear flag and send BootstrapDone so the scheduler
                // still starts rather than hanging forever waiting for the signal.
                bootstrap_active_for_panic.store(false, Ordering::SeqCst);
                let _ = startup_sender_for_panic.send(StartupSignal::BootstrapDone);
            }
        });
    }

    /// UI-thread tail of the bootstrap, run when `StartupSignal::BootstrapDone`
    /// drains. Rebuilds the scheduler's token snapshot from the (now rotated)
    /// config, starts the background refresher, applies the freshly fetched
    /// usage into the profile rows, and performs the one-shot startup
    /// auto-switch. No HTTP — all of this is lock-scoped or in-process.
    fn finish_bootstrap(&mut self) {
        self.refresh_tokens();

        spawn_refresher(
            Arc::clone(&self.config),
            Arc::clone(&self.usage_tokens),
            Arc::clone(&self.usage_store),
            Arc::clone(&self.usage_status),
            Arc::clone(&self.next_refresh_per_profile),
            Arc::clone(&self.activity),
            Arc::clone(&self.last_fetched),
            Arc::clone(&self.pending_window_rotation),
            Arc::clone(&self.last_rotated_window),
            Arc::clone(&self.pending_switch),
            Arc::clone(&self.refetch_queue),
            Arc::clone(&self.learned_intervals),
            Arc::clone(&self.ok_count),
            Arc::clone(&self.cache_hit_count),
            Arc::clone(&self.last_429),
        );

        self.apply_usage();
        let switched = {
            let mut cfg = self.config();
            auto_switch_if_needed(&mut cfg).ok().flatten()
        };
        if let Some(target) = switched {
            self.toast(ToastKind::Warning, format!("auto-switched to '{target}'"));
        }
    }

    pub(crate) fn apply_usage(&mut self) {
        // Fail-safe on a poisoned lock: a fetch worker that panicked under the
        // store lock must not blank every profile's usage. Skip the field whose
        // lock errored and keep the prior value, matching the "poison == no new
        // info" direction used by partition_due / scan_auto_switch. A blanked
        // map here would run every tick (poison is permanent) and blind
        // auto-switch forever.
        let info_map = self.usage_store.lock().ok();
        let status_map = self.usage_status.lock().ok();
        let mut cfg = self.config();
        for p in &mut cfg.profiles {
            if let Some(s) = info_map.as_ref() {
                p.usage = s.get(&p.name).cloned();
            }
            if let Some(s) = status_map.as_ref() {
                p.fetch_status = s.get(&p.name).copied();
            }
        }
    }

    /// Pick up state edits from a concurrent clauth instance (or hand edits
    /// in ~/.clauth/). Returns true if a reload happened.
    pub(crate) fn reload_if_state_changed(&mut self) -> bool {
        let current = app_state_mtime();
        if current == self.last_state_mtime {
            return false;
        }
        if let Ok(fresh) = load_config() {
            *self.config() = fresh;
            self.last_state_mtime = current;
            *self
                .usage_tokens
                .lock()
                .expect("usage_tokens mutex poisoned") = collect_tokens(&self.config().profiles);
            true
        } else {
            false
        }
    }

    pub(crate) fn refresh_tokens(&self) {
        *self
            .usage_tokens
            .lock()
            .expect("usage_tokens mutex poisoned") = collect_tokens(&self.config().profiles);
    }

    pub(crate) fn manual_refresh(&self) {
        let names: Vec<String> = self
            .usage_tokens
            .lock()
            .expect("usage_tokens mutex poisoned")
            .iter()
            .map(|e| e.name.clone())
            .collect();
        if let Ok(mut q) = self.refetch_queue.lock() {
            for name in names {
                q.insert(name);
            }
        }
    }

    pub(crate) fn toast(&mut self, kind: ToastKind, body: impl Into<String>) {
        if self.toasts.len() >= TOAST_CAPACITY {
            self.toasts.pop_front();
        }
        self.toasts.push_back(Toast {
            kind,
            body: body.into(),
            born: Instant::now(),
        });
    }

    pub(crate) fn prune_toasts(&mut self) {
        while let Some(front) = self.toasts.front() {
            if front.born.elapsed() >= TOAST_TTL {
                self.toasts.pop_front();
            } else {
                break;
            }
        }
    }

    // ── Main list ────────────────────────────────────────────────────────────

    pub(crate) fn main_items(&self) -> Vec<MainItemKind> {
        let cfg = self.config();
        let mut items: Vec<MainItemKind> =
            (0..cfg.profiles.len()).map(MainItemKind::Profile).collect();
        if !cfg.profiles.is_empty() {
            items.push(MainItemKind::ActionSeparator);
        }
        items.push(MainItemKind::NewProfile);
        items.push(MainItemKind::CaptureCredentials);
        items.push(MainItemKind::OpenChain);
        items
    }

    pub(crate) fn clamp_main_cursor(&mut self) {
        let items = self.main_items();
        let len = items.len();
        if len == 0 {
            self.main_cursor = 0;
            return;
        }
        if self.main_cursor >= len {
            self.main_cursor = len - 1;
        }
        // Slide off the separator if a delete left the cursor on it.
        while matches!(
            items.get(self.main_cursor),
            Some(MainItemKind::ActionSeparator)
        ) && self.main_cursor > 0
        {
            self.main_cursor -= 1;
        }
    }

    pub(crate) fn current_main_item(&self) -> Option<MainItemKind> {
        self.main_items().get(self.main_cursor).copied()
    }
}

// ── Token snapshot ────────────────────────────────────────────────────────────

pub(crate) fn collect_tokens(profiles: &[Profile]) -> Vec<TokenEntry> {
    profiles
        .iter()
        .filter_map(|p| {
            let oauth = p.credentials.as_ref()?.claude_ai_oauth.as_ref()?;
            Some(TokenEntry {
                name: p.name.clone(),
                access_token: oauth.access_token.clone(),
                refresh_token: oauth.refresh_token.clone(),
                fallback_threshold: p
                    .fallback_threshold
                    .unwrap_or_else(default_fallback_threshold),
            })
        })
        .collect()
}

// ── Startup reconciliation ────────────────────────────────────────────────────

/// Kick off startup credential reconciliation without blocking the first
/// paint. The fast, network-free decision runs inline on the UI thread:
/// read the live `~/.claude/.credentials.json`, compare it to the active
/// profile's stored credentials. In the common no-divergence case we snapshot
/// and signal `ReconcileDone` immediately.
///
/// When the bytes diverge we cannot tell from them alone whether Claude Code
/// silently refreshed (rotating the stored chain) or did a fresh `/login` on a
/// separate chain. The only authoritative liveness test for the stored chain is
/// an OAuth refresh, but a refresh *spends* the single-use refresh token
/// server-side — so probing here would rotate the stored identity on every
/// diverged startup, even when the user goes on to keep it. We therefore never
/// probe: divergence always hands the verdict to the user via the Divergence
/// modal. None of its actions spend the stored token as a side effect (Overwrite
/// takes the live creds, NewProfile captures them into a new profile, Discard
/// relinks the stored creds as-is); a kept-but-stale stored access token is
/// refreshed lazily on the next real fetch, when the user is actually using it.
pub(crate) fn reconcile_startup(app: &mut App) {
    let Some(active) = app.config().state.active_profile.clone() else {
        let _ = app.startup_sender.send(StartupSignal::ReconcileDone);
        return;
    };

    // Read the live credentials under the same lock that mutators use, so a
    // concurrent clauth process mid-rotation can't expose a torn snapshot.
    let live = with_state_lock(|| Ok(read_claude_credentials().ok().flatten()))
        .ok()
        .flatten();
    let diverged = {
        let cfg = app.config();
        let stored = cfg.find(&active).and_then(|p| p.credentials.as_ref());
        credentials_diverged(stored, live.as_ref())
    };

    if !diverged {
        let mut cfg = app.config();
        let _ = snapshot_active_credentials(&mut cfg);
        let _ = app.startup_sender.send(StartupSignal::ReconcileDone);
        return;
    }

    // Diverged: hand the verdict to the user without spending the stored
    // refresh token. No network, no FS write here — the chosen modal action
    // resolves divergence. Inline send keeps startup off the network path.
    let _ = app
        .startup_sender
        .send(StartupSignal::ReconcileNeedsPrompt { active });
}

// ── Event handling ────────────────────────────────────────────────────────────

pub(crate) fn handle_key(app: &mut App, key: KeyEvent) {
    if key.kind != KeyEventKind::Press {
        return;
    }

    // Ctrl-C always exits. Modal stack and screens be damned.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.quit = true;
        return;
    }

    if !app.modals.is_empty() {
        handle_modal_key(app, key);
        return;
    }

    match app.screen {
        Screen::Overview => handle_main_key(app, key),
        Screen::Chain => handle_chain_key(app, key),
        Screen::ProfileDetail { .. } => handle_profile_detail_key(app, key),
    }
}

fn handle_main_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => reorder_main_cursor(app, -1),
        KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => reorder_main_cursor(app, 1),
        KeyCode::Up | KeyCode::Char('k') => step_main_cursor(app, -1),
        KeyCode::Down | KeyCode::Char('j') => step_main_cursor(app, 1),
        KeyCode::Enter => activate_main_item(app),
        KeyCode::Char('m') => open_profile_menu_at_cursor(app),
        KeyCode::Char('d') => open_profile_detail_at_cursor(app),
        KeyCode::Char('f') => {
            app.screen = Screen::Chain;
            app.chain_cursor = 0;
        }
        KeyCode::Char('q') | KeyCode::Esc => {
            app.quit = true;
        }
        KeyCode::Char('?') => app.modals.push(Modal::Help),
        KeyCode::Char('r') => {
            app.manual_refresh();
            app.toast(ToastKind::Info, "refreshing usage…");
        }
        KeyCode::Char('t') => {
            app.modals.push(Modal::Confirm(ConfirmState {
                message: ROTATE_ALL_MSG.to_string(),
                detail: Some(ROTATE_ALL_DETAIL.to_string()),
                choice: false,
                on_confirm: ConfirmAction::RotateAll,
            }));
        }
        _ => {}
    }
}

/// Move the main cursor by one item, wrapping at both ends and skipping
/// over non-selectable rows (currently just `ActionSeparator`).
fn step_main_cursor(app: &mut App, delta: i32) {
    let items = app.main_items();
    let len = items.len();
    if len == 0 {
        return;
    }
    let mut idx = app.main_cursor as i32;
    for _ in 0..len {
        idx = (idx + delta).rem_euclid(len as i32);
        if !matches!(items.get(idx as usize), Some(MainItemKind::ActionSeparator)) {
            break;
        }
    }
    app.main_cursor = idx as usize;
}

fn open_profile_menu_at_cursor(app: &mut App) {
    let Some(MainItemKind::Profile(idx)) = app.current_main_item() else {
        return;
    };
    let Some(name) = app.config().profiles.get(idx).map(|p| p.name.clone()) else {
        return;
    };
    app.modals
        .push(Modal::ProfileMenu(ProfileMenuState { name, cursor: 0 }));
}

fn open_profile_detail_at_cursor(app: &mut App) {
    let Some(MainItemKind::Profile(idx)) = app.current_main_item() else {
        return;
    };
    app.screen = Screen::ProfileDetail { profile_index: idx };
}

fn activate_main_item(app: &mut App) {
    let Some(item) = app.current_main_item() else {
        return;
    };
    match item {
        MainItemKind::Profile(idx) => {
            let cfg = app.config();
            let Some(name) = cfg.profiles.get(idx).map(|p| p.name.clone()) else {
                return;
            };
            // No-op when already active; saves the round-trip through the
            // confirm modal and a redundant token refresh.
            if cfg.is_active(&name) {
                return;
            }
            drop(cfg);
            app.modals.push(Modal::Confirm(ConfirmState {
                message: format!("Switch to '{name}'?"),
                detail: None,
                choice: true,
                on_confirm: ConfirmAction::Switch(name),
            }));
        }
        MainItemKind::ActionSeparator => {}
        MainItemKind::NewProfile => open_new_profile(app),
        MainItemKind::CaptureCredentials => begin_capture(app, false),
        MainItemKind::OpenChain => {
            app.screen = Screen::Chain;
            app.chain_cursor = 0;
        }
    }
}

fn reorder_main_cursor(app: &mut App, delta: i32) {
    // Reorder only acts on real profile rows. Action rows stay anchored at
    // the bottom, regardless of profile count.
    let Some(MainItemKind::Profile(idx)) = app.current_main_item() else {
        return;
    };
    let new_idx = match delta.signum() {
        -1 if idx > 0 => idx - 1,
        1 if idx + 1 < app.config().profiles.len() => idx + 1,
        _ => return,
    };
    let result = {
        let mut cfg = app.config();
        reorder_profile(&mut cfg, idx, new_idx)
    };
    if let Err(e) = result {
        app.toast(ToastKind::Danger, format!("reorder failed: {e}"));
        return;
    }
    // Cursor follows the moved row so the user can keep nudging it.
    if delta < 0 && app.main_cursor > 0 {
        app.main_cursor -= 1;
    } else if delta > 0 {
        app.main_cursor += 1;
    }
}

/// Kick off a TUI switch to `name`. Marks the target `Switching` and spawns a
/// worker that rotates the outgoing active and incoming target profiles via
/// `rotate_one`, then emits `OpResult { kind: Switching, outcome }`. The drain in `on_tick`
/// completes the FS half (`switch_profile` + bookkeeping) when the result
/// lands. Refusal on a non-idle target is the caller's responsibility.
fn perform_switch(app: &mut App, name: &str) {
    mark_activity(&app.activity, name, ProfileActivity::Switching);
    let config = Arc::clone(&app.config);
    let activity = Arc::clone(&app.activity);
    let sender = app.op_sender.clone();
    let target = name.to_string();
    // Read the outgoing active profile before mutating anything — worker only
    // rotates active + target, not every profile.
    let outgoing = app.config().state.active_profile.clone();
    std::thread::spawn(move || {
        // `catch_unwind` ensures the Switching slot is cleared even when a
        // panic fires before the `sender.send` below. Without this, a panic
        // leaves the slot set forever: `any_busy` stays true and ALL future
        // switches are blocked for the lifetime of the process. The
        // `AssertUnwindSafe` wrappers are intentional — these Arcs are
        // shared-mutable cells with their own locks; no per-thread invariant
        // can be violated for other threads by a panic inside this closure.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Rotate only the outgoing active and incoming target profiles.
            // Every other profile's single-use refresh token is left untouched.
            // `rotate_one` returns false (no HTTP) when there is no refresh
            // token or a live session holds the chain — both are safe to skip.
            if let Some(ref active) = outgoing
                && active != &target
            {
                oauth::rotate_one(&config, active, &activity, &sender);
            }
            oauth::rotate_one(&config, &target, &activity, &sender);
            // Re-stamp Switching so the spinner stays up through the FS leg
            // the UI thread runs next (rotate_one leaves the slot as Idle).
            mark_activity(&activity, &target, ProfileActivity::Switching);
            let _ = sender.send(OpResult {
                name: target.clone(),
                kind: ActivityKind::Switching,
                outcome: Ok(()),
            });
        }));
        if result.is_err() {
            // Panic path: clear the slot and send a failure result so the UI
            // thread can toast the error and the switch operation is unblocked.
            clear_activity(&activity, &target);
            let _ = sender.send(OpResult {
                name: target,
                kind: ActivityKind::Switching,
                outcome: Err(anyhow::anyhow!("switch worker panicked")),
            });
        }
    });
}

/// FS half of the TUI switch: runs on the UI thread when an
/// `OpResult { kind: Switching, .. }` drains. No HTTP — safe to keep
/// inline. Clears the Switching marker, runs `switch_profile`, and on
/// success refreshes the scheduler's token snapshot and bumps state mtime.
fn finalize_switch(app: &mut App, name: &str) {
    // Guard a diverged outgoing active before `switch_profile` runs blind.
    // `switch_profile` would no-op the snapshot of the diverged live creds and
    // then `link_profile_credentials(target)` would bail on the regular file,
    // failing the switch and stranding the outgoing profile's fresh `/login`
    // chain (later overwritten by relink/shutdown). The auto-switch path has no
    // other divergence check, so raise the same Divergence modal the 1Hz poll
    // and CLI use: the user resolves the outgoing creds (Overwrite / NewProfile
    // / Discard) and re-triggers the switch from the now-clean link. First-login
    // adoption stays a clean switch (`switch_profile` adopts it).
    let outgoing = app.config().state.active_profile.clone();
    if let Some(active) = outgoing
        && active != name
        && matches!(
            classify_credentials_link(&active).ok(),
            Some(LinkState::Diverged)
        )
        && !is_first_login(&active).unwrap_or(false)
    {
        clear_activity(&app.activity, name);
        app.toast(
            ToastKind::Warning,
            format!("'{active}' has unsaved Claude Code credentials — resolve before switching"),
        );
        app.modals
            .push(Modal::Divergence(DivergenceForm { active, cursor: 0 }));
        return;
    }
    let result = {
        let mut cfg = app.config();
        switch_profile(&mut cfg, name)
    };
    clear_activity(&app.activity, name);
    match result {
        Ok(()) => {
            app.refresh_tokens();
            app.last_state_mtime = app_state_mtime();
            app.toast(ToastKind::Success, format!("switched to '{name}'"));
        }
        Err(e) => app.toast(ToastKind::Danger, format!("switch failed: {e}")),
    }
}

fn open_new_profile(app: &mut App) {
    app.modals.push(Modal::NewProfile(NewProfileForm {
        name: InputState::new(""),
        base_url: InputState::new(""),
        api_key: InputState::new(""),
        focus: NewProfileField::Name,
    }));
}

fn open_edit_profile(app: &mut App, name: &str) {
    let cfg = app.config();
    let Some(profile) = cfg.find(name) else {
        return;
    };
    let modal = Modal::EditProfile(EditProfileForm {
        name: name.to_string(),
        base_url: InputState::new(profile.base_url.as_deref().unwrap_or("")),
        api_key: InputState::new(profile.api_key.as_deref().unwrap_or("")),
        focus: EndpointField::BaseUrl,
    });
    drop(cfg);
    app.modals.push(modal);
}

fn open_delete_confirm(app: &mut App, name: &str) {
    app.modals.push(Modal::Confirm(ConfirmState {
        message: format!("Delete '{name}'?"),
        detail: Some("Profile directory and credentials will be removed.".to_string()),
        choice: false,
        on_confirm: ConfirmAction::Delete(name.to_string()),
    }));
}

fn begin_capture(app: &mut App, from_divergence: bool) {
    let snapshot = match capture_snapshot() {
        Ok(s) => s,
        Err(e) => {
            app.toast(ToastKind::Danger, format!("capture failed: {e}"));
            return;
        }
    };
    let existing_match = {
        let cfg = app.config();
        find_matching_oauth_profile(&cfg, snapshot.credentials.as_ref()).map(str::to_string)
    };
    if let Some(existing) = existing_match {
        app.modals.push(Modal::Confirm(ConfirmState {
            message: format!("These credentials already belong to '{existing}'."),
            detail: Some("Capture anyway?".to_string()),
            choice: false,
            on_confirm: ConfirmAction::CaptureConflict(Box::new(snapshot), from_divergence),
        }));
        return;
    }
    app.modals.push(Modal::CaptureName(CaptureNameForm {
        snapshot: Box::new(snapshot),
        input: InputState::new(""),
        from_divergence,
    }));
}

// ── Chain screen ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub(crate) enum ChainItemKind {
    Member(usize),
    Add,
    Back,
}

pub(crate) fn chain_items(app: &App) -> Vec<ChainItemKind> {
    let cfg = app.config();
    let mut items: Vec<ChainItemKind> = cfg
        .state
        .fallback_chain
        .iter()
        .enumerate()
        .map(|(i, _)| ChainItemKind::Member(i))
        .collect();
    let any_unchained = cfg
        .profiles
        .iter()
        .any(|p| !cfg.state.fallback_chain.iter().any(|c| c == &p.name));
    if any_unchained {
        items.push(ChainItemKind::Add);
    }
    items.push(ChainItemKind::Back);
    items
}

fn handle_chain_key(app: &mut App, key: KeyEvent) {
    let items = chain_items(app);
    let last = items.len().saturating_sub(1);

    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.chain_cursor = if app.chain_cursor == 0 {
                last
            } else {
                app.chain_cursor - 1
            };
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.chain_cursor = if app.chain_cursor >= last {
                0
            } else {
                app.chain_cursor + 1
            };
        }
        KeyCode::Enter => activate_chain_item(app),
        KeyCode::Char('q') | KeyCode::Esc => {
            app.screen = Screen::Overview;
        }
        KeyCode::Char('?') => app.modals.push(Modal::Help),
        KeyCode::Char('r') => {
            app.manual_refresh();
            app.toast(ToastKind::Info, "refreshing usage…");
        }
        _ => {}
    }
}

fn activate_chain_item(app: &mut App) {
    let items = chain_items(app);
    let Some(item) = items.get(app.chain_cursor).copied() else {
        return;
    };
    match item {
        ChainItemKind::Member(i) => {
            let Some(name) = app.config().state.fallback_chain.get(i).cloned() else {
                return;
            };
            app.modals
                .push(Modal::ChainItemMenu(ChainItemMenuState { name, cursor: 0 }));
        }
        ChainItemKind::Add => {
            let candidates: Vec<String> = {
                let cfg = app.config();
                cfg.profiles
                    .iter()
                    .filter(|p| !cfg.state.fallback_chain.iter().any(|c| c == &p.name))
                    .map(|p| p.name.clone())
                    .collect()
            };
            if candidates.is_empty() {
                return;
            }
            app.modals.push(Modal::ChainAdd(ChainAddState {
                candidates,
                cursor: 0,
            }));
        }
        ChainItemKind::Back => {
            app.screen = Screen::Overview;
        }
    }
}

// ── Modal handling ────────────────────────────────────────────────────────────

fn handle_modal_key(app: &mut App, key: KeyEvent) {
    let Some(top) = app.modals.last().cloned() else {
        return;
    };
    match top {
        Modal::Help => {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?' | 'q')
            ) {
                app.modals.pop();
            }
        }
        Modal::NewProfile(_) => handle_new_profile_key(app, key),
        Modal::EditProfile(_) => handle_edit_profile_key(app, key),
        Modal::Rename(_) => handle_rename_key(app, key),
        Modal::Confirm(_) => handle_confirm_key(app, key),
        Modal::Divergence(_) => handle_divergence_key(app, key),
        Modal::CaptureName(_) => handle_capture_name_key(app, key),
        Modal::ProfileMenu(_) => handle_profile_menu_key(app, key),
        Modal::ChainItemMenu(_) => handle_chain_item_menu_key(app, key),
        Modal::ChainAdd(_) => handle_chain_add_key(app, key),
        Modal::ChainThreshold(_) => handle_chain_threshold_key(app, key),
    }
}

fn handle_profile_detail_key(app: &mut App, key: KeyEvent) {
    let Screen::ProfileDetail { profile_index } = app.screen else {
        return;
    };
    let Some(name) = app
        .config()
        .profiles
        .get(profile_index)
        .map(|p| p.name.clone())
    else {
        app.screen = Screen::Overview;
        return;
    };
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.screen = Screen::Overview;
        }
        KeyCode::Char('m') => {
            app.modals
                .push(Modal::ProfileMenu(ProfileMenuState { name, cursor: 0 }));
        }
        KeyCode::Char('r') => {
            app.manual_refresh();
            app.toast(ToastKind::Info, "refreshing usage…");
        }
        KeyCode::Char('?') => app.modals.push(Modal::Help),
        _ => {}
    }
}

fn toggle_auto_start(app: &mut App, name: &str) {
    enum Outcome {
        NotOAuth,
        Saved(bool),
        SaveFailed(anyhow::Error),
        Missing,
    }
    let outcome = {
        let mut cfg = app.config();
        match cfg.find_mut(name) {
            None => Outcome::Missing,
            Some(profile) if !profile.is_oauth() => Outcome::NotOAuth,
            Some(profile) => {
                profile.auto_start = !profile.auto_start;
                let now_on = profile.auto_start;
                match save_profile(profile) {
                    Ok(()) => Outcome::Saved(now_on),
                    Err(e) => {
                        if let Some(p) = cfg.find_mut(name) {
                            p.auto_start = !now_on;
                        }
                        Outcome::SaveFailed(e)
                    }
                }
            }
        }
    };
    match outcome {
        Outcome::Missing => {}
        Outcome::NotOAuth => app.toast(
            ToastKind::Warning,
            "auto-start usage only applies to OAuth profiles",
        ),
        Outcome::Saved(now_on) => {
            // Turning auto-start ON kicks the 5h window right away when the
            // profile has no live window. Toggling is an explicit user action,
            // so clear the 4.5h cooldown first — otherwise a prior (possibly
            // failed) auto-start stamp makes `auto_start_named` silently skip.
            // Then enqueue into `pending_auto_start`; the on_tick drain spawns
            // the AutoStarting worker (refresh + Haiku kick + usage re-fetch).
            // A profile that already has a live window is left alone — the
            // window-expiry rotation path re-arms it when it next resets.
            if now_on {
                let has_window = app
                    .usage_store
                    .lock()
                    .ok()
                    .and_then(|s| {
                        s.get(name).map(|u| {
                            u.five_hour
                                .as_ref()
                                .and_then(|w| w.resets_at.as_ref())
                                .is_some()
                        })
                    })
                    .unwrap_or(false);
                if !has_window {
                    {
                        let mut cfg = app.config();
                        cfg.state.last_auto_start_at.remove(name);
                        let _ = save_app_state(&cfg.state);
                    }
                    if let Ok(mut q) = app.pending_auto_start.lock() {
                        q.insert(name.to_string());
                    }
                }
            }
            let body = if now_on {
                format!("auto-start usage on for '{name}'")
            } else {
                format!("auto-start usage off for '{name}'")
            };
            app.toast(ToastKind::Success, body);
        }
        Outcome::SaveFailed(e) => {
            app.toast(ToastKind::Danger, format!("save failed: {e}"));
        }
    }
}

/// Options shown in the per-profile actions menu. Switching and viewing
/// details are reachable directly from the overview (Enter / d), so this
/// menu stays focused on mutations.
pub(crate) fn profile_menu_options(app: &App, name: &str) -> Vec<ProfileMenuAction> {
    let mut out = Vec::with_capacity(5);
    let is_oauth = app
        .config()
        .find(name)
        .map(|p| p.is_oauth())
        .unwrap_or(false);

    out.push(ProfileMenuAction::Edit);
    out.push(ProfileMenuAction::Rename);
    if is_oauth {
        out.push(ProfileMenuAction::ToggleAutoStart);
    }
    out.push(ProfileMenuAction::Delete);
    out.push(ProfileMenuAction::Back);
    out
}

fn handle_profile_menu_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::ProfileMenu(state)) = app.modals.last().cloned() else {
        return;
    };
    let options = profile_menu_options(app, &state.name);
    let last = options.len().saturating_sub(1);
    let mut cursor = state.cursor.min(last);

    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            cursor = if cursor == 0 { last } else { cursor - 1 };
            if let Some(Modal::ProfileMenu(s)) = app.modals.last_mut() {
                s.cursor = cursor;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            cursor = if cursor >= last { 0 } else { cursor + 1 };
            if let Some(Modal::ProfileMenu(s)) = app.modals.last_mut() {
                s.cursor = cursor;
            }
        }
        KeyCode::Esc => {
            app.modals.pop();
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let Some(&action) = options.get(cursor) else {
                return;
            };
            run_profile_menu_action(app, &state.name, action);
        }
        _ => {}
    }
}

fn run_profile_menu_action(app: &mut App, name: &str, action: ProfileMenuAction) {
    match action {
        ProfileMenuAction::Edit => {
            app.modals.pop();
            open_edit_profile(app, name);
        }
        ProfileMenuAction::Rename => {
            app.modals.pop();
            app.modals.push(Modal::Rename(RenameForm {
                old: name.to_string(),
                input: InputState::new(name),
            }));
        }
        ProfileMenuAction::ToggleAutoStart => {
            // Stay in the menu so the user can flip several settings.
            toggle_auto_start(app, name);
        }
        ProfileMenuAction::Delete => {
            app.modals.pop();
            open_delete_confirm(app, name);
        }
        ProfileMenuAction::Back => {
            app.modals.pop();
        }
    }
}

fn handle_new_profile_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::NewProfile(form)) = app.modals.last_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => {
            app.modals.pop();
        }
        KeyCode::Tab | KeyCode::Down => {
            form.focus = match form.focus {
                NewProfileField::Name => NewProfileField::BaseUrl,
                NewProfileField::BaseUrl => NewProfileField::ApiKey,
                NewProfileField::ApiKey => NewProfileField::Name,
            };
        }
        KeyCode::BackTab | KeyCode::Up => {
            form.focus = match form.focus {
                NewProfileField::Name => NewProfileField::ApiKey,
                NewProfileField::BaseUrl => NewProfileField::Name,
                NewProfileField::ApiKey => NewProfileField::BaseUrl,
            };
        }
        KeyCode::Enter => {
            submit_new_profile(app);
        }
        _ => {
            let input = match form.focus {
                NewProfileField::Name => &mut form.name,
                NewProfileField::BaseUrl => &mut form.base_url,
                NewProfileField::ApiKey => &mut form.api_key,
            };
            apply_input_edit(input, key);
        }
    }
}

fn submit_new_profile(app: &mut App) {
    let Some(Modal::NewProfile(form)) = app.modals.last() else {
        return;
    };
    let name = form.name.trimmed().to_string();
    let base_url = form.base_url.trimmed_some();
    let api_key = form.api_key.trimmed_some();
    let validation = {
        let cfg = app.config();
        let existing = cfg.names();
        validate_profile_name(&name, &existing, None)
    };
    if let Err(e) = validation {
        app.toast(ToastKind::Danger, format!("{e}"));
        return;
    }
    // API key only makes sense for endpoint profiles. Drop it if no URL.
    let api_key = if base_url.is_some() { api_key } else { None };
    let result = {
        let mut cfg = app.config();
        create_blank_profile(&mut cfg, name.clone(), base_url, api_key)
    };
    match result {
        Ok(()) => {
            app.refresh_tokens();
            app.last_state_mtime = app_state_mtime();
            app.modals.pop();
            app.toast(ToastKind::Success, format!("created '{name}'"));
        }
        Err(e) => app.toast(ToastKind::Danger, format!("create failed: {e}")),
    }
}

fn handle_edit_profile_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::EditProfile(form)) = app.modals.last_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => {
            app.modals.pop();
        }
        KeyCode::Tab | KeyCode::Down | KeyCode::Up | KeyCode::BackTab => {
            form.focus = match form.focus {
                EndpointField::BaseUrl => EndpointField::ApiKey,
                EndpointField::ApiKey => EndpointField::BaseUrl,
            };
        }
        KeyCode::Enter => {
            submit_edit_profile(app);
        }
        _ => {
            let input = match form.focus {
                EndpointField::BaseUrl => &mut form.base_url,
                EndpointField::ApiKey => &mut form.api_key,
            };
            apply_input_edit(input, key);
        }
    }
}

fn submit_edit_profile(app: &mut App) {
    let Some(Modal::EditProfile(form)) = app.modals.last() else {
        return;
    };
    let name = form.name.clone();
    let base_url = form.base_url.trimmed_some();
    let api_key = if base_url.is_some() {
        form.api_key.trimmed_some()
    } else {
        None
    };
    let result = {
        let mut cfg = app.config();
        edit_profile_endpoint(&mut cfg, &name, base_url, api_key)
    };
    match result {
        Ok(()) => {
            app.modals.pop();
            app.toast(ToastKind::Success, format!("updated '{name}'"));
        }
        Err(e) => app.toast(ToastKind::Danger, format!("edit failed: {e}")),
    }
}

fn handle_rename_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::Rename(form)) = app.modals.last_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => {
            app.modals.pop();
        }
        KeyCode::Enter => {
            let new = form.input.trimmed().to_string();
            let old = form.old.clone();
            if new == old {
                app.modals.pop();
                return;
            }
            let validation = {
                let cfg = app.config();
                let existing = cfg.names();
                validate_profile_name(&new, &existing, Some(old.as_str()))
            };
            if let Err(e) = validation {
                app.toast(ToastKind::Danger, format!("{e}"));
                return;
            }
            let result = {
                let mut cfg = app.config();
                rename_profile(&mut cfg, &old, &new)
            };
            match result {
                Ok(()) => {
                    app.refresh_tokens();
                    app.last_state_mtime = app_state_mtime();
                    app.modals.pop();
                    app.toast(ToastKind::Success, format!("renamed '{old}' → '{new}'"));
                }
                Err(e) => app.toast(ToastKind::Danger, format!("rename failed: {e}")),
            }
        }
        _ => apply_input_edit(&mut form.input, key),
    }
}

fn handle_confirm_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::Confirm(state)) = app.modals.last_mut() else {
        return;
    };
    match key.code {
        KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::Char('h' | 'l') => {
            state.choice = !state.choice;
        }
        KeyCode::Char('y') => state.choice = true,
        KeyCode::Char('n') => state.choice = false,
        KeyCode::Esc => {
            app.modals.pop();
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let confirmed = state.choice;
            let action = state.on_confirm.clone();
            app.modals.pop();
            if confirmed {
                run_confirm_action(app, action);
            }
        }
        _ => {}
    }
}

fn run_confirm_action(app: &mut App, action: ConfirmAction) {
    match action {
        ConfirmAction::Delete(name) => {
            let result = {
                let mut cfg = app.config();
                delete_profile(&mut cfg, &name)
            };
            match result {
                Ok(()) => {
                    app.refresh_tokens();
                    app.last_state_mtime = app_state_mtime();
                    app.clamp_main_cursor();
                    app.toast(ToastKind::Success, format!("deleted '{name}'"));
                }
                Err(e) => app.toast(ToastKind::Danger, format!("delete failed: {e}")),
            }
        }
        ConfirmAction::CaptureConflict(snapshot, from_divergence) => {
            app.modals.push(Modal::CaptureName(CaptureNameForm {
                snapshot,
                input: InputState::new(""),
                from_divergence,
            }));
        }
        ConfirmAction::Switch(name) => {
            // Block a second switch while the previous one is still mid-flight.
            if !is_idle(&app.activity, &name) {
                app.toast(
                    ToastKind::Warning,
                    format!("'{name}' is already busy — try again in a moment"),
                );
                return;
            }
            perform_switch(app, &name);
        }
        ConfirmAction::DiscardDivergence(name) => run_discard_divergence(app, &name),
        ConfirmAction::RotateAll => {
            // Refuse if anything is already in flight — a parallel rotate-all
            // worker would step on per-profile work or duplicate a refresh
            // already mid-rotation. Bootstrap is a whole-worker busy signal
            // separate from per-profile activity: between the last Refreshing
            // slot clearing and the first AutoStarting slot setting there is a
            // window where activity_store is empty but the bootstrap worker is
            // still running auto_start_windows. The flag covers that gap.
            if app.bootstrap_active.load(Ordering::SeqCst) || any_busy(&app.activity) {
                app.toast(
                    ToastKind::Warning,
                    "rotate-all skipped — another op is still in flight",
                );
                return;
            }
            // Spawn the rotate-all worker so HTTP runs off the UI thread.
            // The worker locks the config only across its brief snapshot /
            // persist windows; per-profile spinners clear as each profile's
            // HTTP completes via the OpResult channel drained in on_tick.
            // A toast confirms the kick-off; per-profile errors surface
            // through the standard drain.
            let config = Arc::clone(&app.config);
            let refetch = Arc::clone(&app.refetch_queue);
            let activity = Arc::clone(&app.activity);
            let sender = app.op_sender.clone();
            std::thread::spawn(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _ = oauth::refresh_all(&config, true, &refetch, &activity, &sender);
                }));
            });
            app.toast(ToastKind::Info, "rotating all tokens…");
        }
    }
}

fn handle_divergence_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::Divergence(state)) = app.modals.last_mut() else {
        return;
    };
    let options = DivergenceForm::options();
    let last = options.len() - 1;
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            state.cursor = if state.cursor == 0 {
                last
            } else {
                state.cursor - 1
            };
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.cursor = if state.cursor >= last {
                0
            } else {
                state.cursor + 1
            };
        }
        KeyCode::Esc => {
            // Esc dismisses without acting. The 1Hz poll re-pushes the modal
            // on the next tick if the divergence persists.
            app.modals.pop();
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let choice = options[state.cursor];
            let active = state.active.clone();
            app.modals.pop();
            run_divergence_choice(app, &active, choice);
        }
        _ => {}
    }
}

fn run_divergence_choice(app: &mut App, active: &str, choice: DivergenceChoice) {
    match choice {
        DivergenceChoice::Overwrite => {
            let snapshot_result = {
                let mut cfg = app.config();
                force_snapshot_active_credentials(&mut cfg)
            };
            if let Err(e) = snapshot_result {
                app.toast(ToastKind::Danger, format!("overwrite failed: {e}"));
                return;
            }
            if let Err(e) = force_link_profile_credentials(active) {
                app.toast(ToastKind::Danger, format!("relink failed: {e}"));
                return;
            }
            app.refresh_tokens();
            app.toast(
                ToastKind::Success,
                format!("saved live credentials into '{active}'"),
            );
        }
        DivergenceChoice::NewProfile => {
            // Defer detaching the live link and deactivating the
            // previously-active profile until the capture actually succeeds
            // (handled in `handle_capture_name_key`'s success arm). Doing it
            // here would silently deactivate the active profile and drop the
            // live link if the user cancels the name modal with Esc.
            begin_capture(app, true);
        }
        DivergenceChoice::Discard => {
            app.modals.push(Modal::Confirm(ConfirmState {
                message: format!("Discard the new login and restore '{active}'?"),
                detail: Some(
                    "Claude Code's freshly written credentials will be overwritten with the profile's stored tokens.".to_string(),
                ),
                choice: false,
                on_confirm: ConfirmAction::DiscardDivergence(active.to_string()),
            }));
        }
    }
}

fn run_discard_divergence(app: &mut App, active: &str) {
    if let Err(e) = force_link_profile_credentials(active) {
        app.toast(ToastKind::Danger, format!("discard failed: {e}"));
        return;
    }
    app.toast(
        ToastKind::Warning,
        format!("discarded new login; restored '{active}'"),
    );
}

fn handle_capture_name_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::CaptureName(form)) = app.modals.last_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => {
            app.modals.pop();
        }
        KeyCode::Enter => {
            let name = form.input.trimmed().to_string();
            let validation = {
                let cfg = app.config();
                let existing = cfg.names();
                validate_profile_name(&name, &existing, None)
            };
            if let Err(e) = validation {
                app.toast(ToastKind::Danger, format!("{e}"));
                return;
            }
            // Consume the boxed snapshot out of the modal.
            let Some(Modal::CaptureName(form)) = app.modals.pop() else {
                return;
            };
            let snapshot = *form.snapshot;
            // Divergence-originated capture: only now that the name is
            // confirmed do we detach the live link and deactivate the
            // previously-active profile, so `capture_into_profile` observes
            // `active_profile.is_none()` and links + activates the new one.
            // On Esc/cancel this never ran, so the prior profile stays linked.
            if form.from_divergence {
                let _ = detach_credentials_link();
                let mut cfg = app.config();
                cfg.state.active_profile = None;
                let _ = save_app_state(&cfg.state);
            }
            let result = {
                let mut cfg = app.config();
                capture_into_profile(&mut cfg, name.clone(), snapshot)
            };
            match result {
                Ok(()) => {
                    app.refresh_tokens();
                    app.last_state_mtime = app_state_mtime();
                    app.toast(ToastKind::Success, format!("captured '{name}'"));
                }
                Err(e) => app.toast(ToastKind::Danger, format!("capture failed: {e}")),
            }
        }
        _ => apply_input_edit(&mut form.input, key),
    }
}

fn handle_chain_item_menu_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::ChainItemMenu(state)) = app.modals.last_mut() else {
        return;
    };
    let (chain_len, position) = {
        let cfg = app.config.lock().expect("config mutex poisoned");
        let chain_len = cfg.state.fallback_chain.len();
        let position = cfg
            .state
            .fallback_chain
            .iter()
            .position(|n| n == &state.name);
        (chain_len, position)
    };
    let mut options: Vec<ChainAction> = vec![ChainAction::Threshold];
    if matches!(position, Some(p) if p > 0) {
        options.push(ChainAction::MoveUp);
    }
    if matches!(position, Some(p) if p + 1 < chain_len) {
        options.push(ChainAction::MoveDown);
    }
    options.push(ChainAction::Remove);
    options.push(ChainAction::Back);
    let last = options.len() - 1;
    if state.cursor > last {
        state.cursor = last;
    }

    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            state.cursor = if state.cursor == 0 {
                last
            } else {
                state.cursor - 1
            };
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.cursor = if state.cursor >= last {
                0
            } else {
                state.cursor + 1
            };
        }
        KeyCode::Esc => {
            app.modals.pop();
        }
        KeyCode::Enter => {
            let name = state.name.clone();
            let action = options[state.cursor];
            match action {
                ChainAction::Threshold => {
                    let current = app
                        .config
                        .lock()
                        .expect("config mutex poisoned")
                        .find(&name)
                        .map(threshold_for)
                        .unwrap_or(DEFAULT_THRESHOLD);
                    app.modals.pop();
                    app.modals.push(Modal::ChainThreshold(ChainThresholdForm {
                        name,
                        input: InputState::new(&format!("{current:.0}")),
                    }));
                }
                ChainAction::MoveUp => {
                    if let Some(p) = position
                        && p > 0
                    {
                        let mut cfg = app.config.lock().expect("config mutex poisoned");
                        cfg.state.fallback_chain.swap(p - 1, p);
                        let _ = save_app_state(&cfg.state);
                        drop(cfg);
                        if app.chain_cursor > 0 {
                            app.chain_cursor -= 1;
                        }
                    }
                }
                ChainAction::MoveDown => {
                    if let Some(p) = position
                        && p + 1 < chain_len
                    {
                        let mut cfg = app.config.lock().expect("config mutex poisoned");
                        cfg.state.fallback_chain.swap(p, p + 1);
                        let _ = save_app_state(&cfg.state);
                        drop(cfg);
                        if app.chain_cursor + 1 < chain_items(app).len() {
                            app.chain_cursor += 1;
                        }
                    }
                }
                ChainAction::Remove => {
                    {
                        let mut cfg = app.config.lock().expect("config mutex poisoned");
                        cfg.state.fallback_chain.retain(|n| n != &name);
                        let _ = save_app_state(&cfg.state);
                    }
                    app.modals.pop();
                    let items_len = chain_items(app).len();
                    if app.chain_cursor >= items_len {
                        app.chain_cursor = items_len.saturating_sub(1);
                    }
                }
                ChainAction::Back => {
                    app.modals.pop();
                }
            }
        }
        _ => {}
    }
}

fn handle_chain_add_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::ChainAdd(state)) = app.modals.last_mut() else {
        return;
    };
    let last = state.candidates.len().saturating_sub(1);
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            state.cursor = if state.cursor == 0 {
                last
            } else {
                state.cursor - 1
            };
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.cursor = if state.cursor >= last {
                0
            } else {
                state.cursor + 1
            };
        }
        KeyCode::Esc => {
            app.modals.pop();
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let Some(name) = state.candidates.get(state.cursor).cloned() else {
                return;
            };
            {
                let mut cfg = app.config.lock().expect("config mutex poisoned");
                if let Some(profile) = cfg.find_mut(&name)
                    && profile.fallback_threshold.is_none()
                {
                    profile.fallback_threshold = Some(DEFAULT_THRESHOLD);
                    let _ = save_profile(profile);
                }
                cfg.state.fallback_chain.push(name);
                let _ = save_app_state(&cfg.state);
            }
            app.modals.pop();
        }
        _ => {}
    }
}

fn handle_chain_threshold_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::ChainThreshold(form)) = app.modals.last_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => {
            app.modals.pop();
        }
        KeyCode::Enter => {
            let raw = form.input.trimmed();
            let Ok(value) = raw.parse::<f64>() else {
                app.toast(ToastKind::Danger, "enter a number between 0 and 100");
                return;
            };
            if !(0.0..=100.0).contains(&value) {
                app.toast(ToastKind::Danger, "threshold must be between 0 and 100");
                return;
            }
            let name = form.name.clone();
            let save_result: Option<anyhow::Error> = {
                let mut cfg = app.config.lock().expect("config mutex poisoned");
                if let Some(profile) = cfg.find_mut(&name) {
                    profile.fallback_threshold = Some(value);
                    save_profile(profile).err()
                } else {
                    None
                }
            };
            if let Some(e) = save_result {
                app.toast(ToastKind::Danger, format!("save failed: {e}"));
                return;
            }
            app.modals.pop();
            app.toast(
                ToastKind::Success,
                format!("threshold for '{name}' set to {value:.0}%"),
            );
        }
        _ => apply_input_edit(&mut form.input, key),
    }
}

fn apply_input_edit(input: &mut InputState, key: KeyEvent) {
    match key.code {
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => input.insert(c),
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete(),
        KeyCode::Left => input.left(),
        KeyCode::Right => input.right(),
        KeyCode::Home => input.home(),
        KeyCode::End => input.end(),
        _ => {}
    }
}

// ── Per-tick maintenance ──────────────────────────────────────────────────────

pub(crate) fn on_tick(app: &mut App) {
    // Advance the spinner frame. Wraps naturally on the 10-frame braille set.
    app.tick_count = app.tick_count.wrapping_add(1);

    // Drain completed op results posted by workers. Each result clears the
    // profile's activity slot back to Idle, surfaces errors as toasts, and —
    // for op kinds where the user wants confirmation — emits a success toast
    // and any follow-up bookkeeping that touches `AppConfig` (refresh tokens,
    // kick off a usage re-fetch).
    let mut drained: Vec<OpResult> = Vec::new();
    while let Ok(result) = app.op_results.try_recv() {
        drained.push(result);
    }
    // Set when any Refreshing or AutoStarting OpResult succeeded; triggers
    // `refresh_tokens()` to rebuild the scheduler's TokenList snapshot so
    // the next fetch uses the rotated access tokens.
    let mut needs_token_snapshot_rebuild = false;
    // Names of profiles whose auto-start completed successfully this tick.
    // These are pushed into RefetchQueue rather than triggering an all-profile
    // manual_refresh — only the auto-started profiles need an immediate re-fetch,
    // and the scheduler's forced-merge path respects Switching/Refreshing
    // exclusions, keeping AIMD the single cadence authority.
    let mut auto_started_names: Vec<String> = Vec::new();
    // Names whose `Switching` OpResult arrived — the FS half runs after the
    // drain loop so the per-name `Refreshing` toasts/clears for the same
    // tick have already been processed and the spinner stays Switching
    // through the relink. (The worker re-stamps Switching after
    // `refresh_all` returns; the drain's conditional clear below preserves
    // that re-stamp against late `Refreshing` results.)
    let mut switch_finalize: Vec<String> = Vec::new();
    for OpResult {
        name,
        kind,
        outcome,
    } in drained
    {
        // Only clear the slot when it still reflects this op's kind. The
        // switch worker re-stamps Switching after `refresh_all` returns; an
        // in-flight Refreshing result for the same name must not clobber
        // that stamp, or the spinner would blink Idle between the refresh
        // leg and the relink leg.
        //
        // Invariant: `ActivityKind::Fetching` is NEVER sent through OpResult.
        // `fetch_with_rotation` and `run_fetch` manage the Fetching activity
        // slot directly (mark before spawn, clear in join loop) without going
        // through the OpResult channel. Valid kinds in OpResult: Refreshing,
        // AutoStarting, Switching.
        if matches!(kind, ActivityKind::Fetching) {
            unreachable!(
                "ActivityKind::Fetching must never be sent via OpResult; \
                 the Fetching slot is managed by the join loop directly"
            );
        }
        if let Ok(mut a) = app.activity.lock()
            && a.get(&name).copied() == Some(kind.as_activity())
        {
            a.remove(&name);
        }
        match outcome {
            Ok(()) => match kind {
                ActivityKind::AutoStarting => {
                    needs_token_snapshot_rebuild = true;
                    auto_started_names.push(name.clone());
                    app.toast(
                        ToastKind::Info,
                        format!("auto-started usage window for '{name}'"),
                    );
                }
                ActivityKind::Refreshing => {
                    needs_token_snapshot_rebuild = true;
                    app.toast(ToastKind::Info, format!("rotated token for '{name}'"));
                }
                ActivityKind::Switching => {
                    switch_finalize.push(name.clone());
                }
                _ => {}
            },
            Err(e) => {
                let verb = match kind {
                    ActivityKind::Fetching => {
                        unreachable!("ActivityKind::Fetching must never be sent via OpResult")
                    }
                    ActivityKind::Refreshing => "refresh",
                    ActivityKind::Switching => "switch",
                    ActivityKind::Starting => "start",
                    ActivityKind::AutoStarting => "auto-start",
                };
                app.toast(
                    ToastKind::Danger,
                    format!("{verb} for '{name}' failed: {e}"),
                );
            }
        }
    }
    // Run the FS half for every successful Switching result. `finalize_switch`
    // clears the Switching marker, runs `switch_profile`, and on success bumps
    // mtime + refreshes the token snapshot. A no-op target is harmless —
    // `switch_profile` returns early when already active.
    for name in switch_finalize {
        finalize_switch(app, &name);
    }
    if needs_token_snapshot_rebuild {
        app.refresh_tokens();
    }
    // Route auto-start re-fetches through RefetchQueue so only the auto-started
    // profiles get an immediate re-fetch, not every profile. The scheduler's
    // forced-merge path picks them up on the next tick, respecting
    // Switching/Refreshing exclusions and keeping AIMD the single cadence
    // authority. This replaces the prior all-profile manual_refresh which was
    // a full double-fetch that raced the scheduler's next tick and injected a
    // false cache-hit signal.
    if !auto_started_names.is_empty()
        && let Ok(mut q) = app.refetch_queue.lock()
    {
        for n in auto_started_names {
            q.insert(n);
        }
    }

    if app.reload_if_state_changed() {
        app.clamp_main_cursor();
    }
    app.apply_usage();

    // Drain the auto-start queue the refresher fills when a successful fetch
    // shows no live 5h window. Each pending name is handed to a worker thread
    // so the HTTP runs off the UI thread; the result is surfaced through the
    // standard OpResult channel (drained at the top of the next tick) so the
    // spinner clears in arrival order. `auto_start_named` enforces its own
    // per-profile cooldown, so a duplicate enqueue is a no-op.
    //
    // Skip the entire drain while the bootstrap worker is running: it may be
    // mid-`auto_start_windows` for these same profiles, and draining here
    // would race on the same single-use refresh tokens. Entries left in the
    // mutex are picked up on the next tick once the flag clears.
    let pending: Vec<String> = if app.bootstrap_active.load(Ordering::SeqCst) {
        Vec::new()
    } else {
        app.pending_auto_start
            .lock()
            .map(|mut g| {
                let v: Vec<String> = g.iter().cloned().collect();
                g.clear();
                v
            })
            .unwrap_or_default()
    };
    for name in pending {
        if !is_idle(&app.activity, &name) {
            continue;
        }
        let config = Arc::clone(&app.config);
        let refetch = Arc::clone(&app.refetch_queue);
        let activity = Arc::clone(&app.activity);
        let sender = app.op_sender.clone();
        let name_for_panic = name.clone();
        let activity_for_panic = Arc::clone(&app.activity);
        let sender_for_panic = app.op_sender.clone();
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = oauth::auto_start_named(&config, &name, &refetch, &activity, &sender);
            }));
            if result.is_err() {
                clear_activity(&activity_for_panic, &name_for_panic);
                let _ = sender_for_panic.send(OpResult {
                    name: name_for_panic,
                    kind: ActivityKind::AutoStarting,
                    outcome: Err(anyhow::anyhow!("auto-start worker panicked")),
                });
            }
        });
    }

    // Drain 5h-window-expiry rotation requests posted by the scheduler. Each
    // entry is handed to a worker that runs the rotation off the UI thread.
    // The map value is the `resets_at` epoch pinned at detection time; using
    // it (not a re-read from `usage_store`) prevents stamping the wrong window
    // if the API returned a fresh `resets_at` between detection and drain.
    //
    // `LastRotatedWindow` and `RefetchQueue` are independent mutexes (not
    // AppConfig), so the worker stamps them inline on success — no UI-side
    // bookkeeping is required after the OpResult lands.
    //
    // Skip the entire drain while bootstrap is running for the same reason as
    // `pending_auto_start` above — entries stay in the mutex for the next tick.
    let pending_rotations: Vec<(String, i64)> = if app.bootstrap_active.load(Ordering::SeqCst) {
        Vec::new()
    } else {
        app.pending_window_rotation
            .lock()
            .map(|mut g| g.drain().collect())
            .unwrap_or_default()
    };
    for (name, epoch) in pending_rotations {
        if !is_idle(&app.activity, &name) {
            continue;
        }
        let config = Arc::clone(&app.config);
        let activity = Arc::clone(&app.activity);
        let refetch = Arc::clone(&app.refetch_queue);
        let last_rotated = Arc::clone(&app.last_rotated_window);
        let sender = app.op_sender.clone();
        let name_for_panic = name.clone();
        let activity_for_panic = Arc::clone(&app.activity);
        let sender_for_panic = app.op_sender.clone();
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let rotated = oauth::rotate_one_for_window(
                    &config,
                    &name,
                    &activity,
                    &sender,
                    &last_rotated,
                    epoch,
                );
                if rotated {
                    // The 5h window just expired and the token was refreshed.
                    // Opted-in OAuth profiles re-arm the window immediately with
                    // the freshly rotated access token (kick-only, no second
                    // refresh): `start_window` shows the AutoStarting spinner,
                    // fires the Haiku ping, and re-fetches usage on success.
                    // Non-opted-in profiles just re-fetch as before.
                    let auto = {
                        let cfg = config.lock().expect("config mutex poisoned");
                        cfg.find(&name)
                            .map(|p| p.auto_start && p.is_oauth())
                            .unwrap_or(false)
                    };
                    if auto {
                        oauth::start_window(&config, &name, &refetch, &activity, &sender);
                    } else if let Ok(mut q) = refetch.lock() {
                        q.insert(name);
                    }
                }
            }));
            if result.is_err() {
                clear_activity(&activity_for_panic, &name_for_panic);
                let _ = sender_for_panic.send(OpResult {
                    name: name_for_panic,
                    kind: ActivityKind::Refreshing,
                    outcome: Err(anyhow::anyhow!("window-rotation worker panicked")),
                });
            }
        });
    }

    // Drain scheduler-posted auto-switch decisions. Each entry was computed
    // by `scan_auto_switch` in the scheduler thread; the UI thread just
    // dispatches the standard switch worker pipeline so the refresh + relink
    // run off the main loop. Skip dispatch when the target is non-idle —
    // either someone clicked switch already or a previous decision is still
    // mid-flight.
    let auto_switch_targets: Vec<String> = app
        .pending_switch
        .lock()
        .map(|mut g| g.drain().collect())
        .unwrap_or_default();
    for name in auto_switch_targets {
        if !is_idle(&app.activity, &name) {
            continue;
        }
        app.toast(ToastKind::Warning, format!("auto-switching to '{name}'"));
        perform_switch(app, &name);
    }

    drain_startup_signals(app);
    maybe_spawn_bootstrap(app);

    poll_credentials_divergence(app);

    app.prune_toasts();
}

/// Drain the startup phase signals posted by the reconcile / bootstrap
/// workers. Reconcile signals flip `reconcile_done` (and may push the
/// Divergence prompt); the bootstrap-done signal runs the UI-thread tail.
fn drain_startup_signals(app: &mut App) {
    while let Ok(signal) = app.startup_results.try_recv() {
        match signal {
            StartupSignal::ReconcileDone => {
                app.reconcile_done = true;
            }
            StartupSignal::ReconcileNeedsPrompt { active } => {
                app.reconcile_done = true;
                app.modals
                    .push(Modal::Divergence(DivergenceForm { active, cursor: 0 }));
            }
            StartupSignal::BootstrapDone => {
                app.finish_bootstrap();
            }
        }
    }
}

/// Spawn the background bootstrap once reconciliation has settled and any
/// reconcile prompt has been answered. Mirrors the old `!bootstrapped &&
/// modals.is_empty()` gate, now keyed off the async reconcile verdict so the
/// HTTP never blocks the first paint.
fn maybe_spawn_bootstrap(app: &mut App) {
    if app.bootstrap_started || !app.reconcile_done || !app.modals.is_empty() {
        return;
    }
    app.bootstrap_started = true;
    app.bootstrap_active.store(true, Ordering::SeqCst);
    app.spawn_bootstrap();
}

/// 1Hz check that the live `.credentials.json` still points at the active
/// profile's stored credentials. Pushes a Divergence modal when CC has
/// overwritten the symlink (typically by `/login`). Skips when any modal is
/// already open so we don't stack on top of work the user has in flight.
fn poll_credentials_divergence(app: &mut App) {
    const POLL_INTERVAL: Duration = Duration::from_secs(1);

    if app.last_divergence_check.elapsed() < POLL_INTERVAL {
        return;
    }
    app.last_divergence_check = Instant::now();

    if !app.modals.is_empty() {
        return;
    }
    let Some(active) = app.config().state.active_profile.clone() else {
        return;
    };
    if !matches!(
        classify_credentials_link(&active).ok(),
        Some(LinkState::Diverged)
    ) {
        return;
    }
    // A credential-less profile's first login isn't a real divergence — adopt
    // Claude Code's write into the profile silently instead of prompting.
    if is_first_login(&active).unwrap_or(false) {
        let result = {
            let mut cfg = app.config();
            adopt_first_login(&mut cfg, &active)
        };
        match result {
            Ok(()) => {
                app.refresh_tokens();
                app.last_state_mtime = app_state_mtime();
                app.toast(ToastKind::Success, format!("saved login into '{active}'"));
            }
            Err(e) => app.toast(ToastKind::Danger, format!("adopt failed: {e}")),
        }
        return;
    }
    app.modals
        .push(Modal::Divergence(DivergenceForm { active, cursor: 0 }));
}

// ── Shutdown ──────────────────────────────────────────────────────────────────

/// Persist whatever Claude Code wrote during this session, then replace the
/// symlink with a plain copy. After shutdown any external write to
/// ~/.claude/.credentials.json lands in that standalone file instead of
/// mutating the active profile's storage through the link.
///
/// AIMD learner state is persisted here — the maps are advisory so writing
/// only on clean shutdown is sufficient; a crash just means the next startup
/// relearns from NORMAL.
pub(crate) fn shutdown(app: &mut App) -> Result<()> {
    {
        let mut cfg = app.config();
        let _ = snapshot_active_credentials(&mut cfg);
        // Flush AIMD learner state so cadence survives clean restarts.
        if let Ok(li) = app.learned_intervals.lock() {
            cfg.state.learned_intervals_ms = li.clone();
        }
        if let Ok(ok) = app.ok_count.lock() {
            cfg.state.consecutive_ok_count = ok.clone();
        }
        if let Ok(l4) = app.last_429.lock() {
            cfg.state.last_429_at = l4.clone();
        }
        if let Ok(ch) = app.cache_hit_count.lock() {
            cfg.state.consecutive_cache_hit_count = ch.clone();
        }
        let _ = save_app_state(&cfg.state);
    }
    let _ = detach_credentials_link();
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use crate::usage::{ActivityStore, ProfileActivity, any_busy};

    fn make_activity(entries: &[(&str, ProfileActivity)]) -> ActivityStore {
        let mut map = HashMap::new();
        for (name, activity) in entries {
            map.insert(name.to_string(), *activity);
        }
        Arc::new(Mutex::new(map))
    }

    fn bootstrap_busy(flag: &Arc<AtomicBool>, activity: &ActivityStore) -> bool {
        flag.load(Ordering::SeqCst) || any_busy(activity)
    }

    #[test]
    fn bootstrap_active_true_reports_busy() {
        let flag = Arc::new(AtomicBool::new(true));
        let activity = make_activity(&[]);
        assert!(bootstrap_busy(&flag, &activity));
    }

    #[test]
    fn bootstrap_active_false_empty_store_reports_idle() {
        let flag = Arc::new(AtomicBool::new(false));
        let activity = make_activity(&[]);
        assert!(!bootstrap_busy(&flag, &activity));
    }

    #[test]
    fn bootstrap_active_true_with_refreshing_slot_reports_busy() {
        let flag = Arc::new(AtomicBool::new(true));
        let activity = make_activity(&[("alice", ProfileActivity::Refreshing)]);
        assert!(bootstrap_busy(&flag, &activity));
    }

    #[test]
    fn bootstrap_active_false_with_refreshing_slot_still_busy() {
        let flag = Arc::new(AtomicBool::new(false));
        let activity = make_activity(&[("alice", ProfileActivity::Refreshing)]);
        assert!(bootstrap_busy(&flag, &activity));
    }
}
