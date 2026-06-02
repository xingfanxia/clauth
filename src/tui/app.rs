//! Application state, keymap, and tick logic.
//!
//! Layout invariants:
//!   - The Overview menu is a read-only list of account rows; `main_cursor`
//!     indexes it. Account creation and editing live on the Config tab.
//!   - The Config tab is master-detail: an account list (plus a `+ new` row)
//!     and an inline detail editor (`config_draft`) — no popups for new / edit
//!     / rename / delete.
//!   - The Fallback tab is master-detail too: the ordered chain (plus a
//!     `+ add` row) on the left, an inline threshold stepper / remove row (or
//!     add-candidate picker) on the right — no popups for threshold / reorder
//!     / remove / add.
//!   - Modals stack: the top of `modals` owns input; events fall through to
//!     the screen below only when the stack is empty.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::actions::{
    CaptureSnapshot, capture_into_profile, capture_snapshot, create_blank_profile, delete_profile,
    edit_profile_endpoint, find_matching_oauth_profile, rename_profile, reorder_profile,
    switch_off, switch_profile, validate_profile_name,
};
use crate::claude::{
    LinkState, adopt_first_login, classify_credentials_link, credentials_diverged,
    detach_credentials_link, force_link_profile_credentials, force_snapshot_active_credentials,
    is_first_login, link_profile_credentials, read_claude_credentials, snapshot_active_credentials,
};
use crate::fallback::{DEFAULT_THRESHOLD, SwitchAction, auto_switch_if_needed, threshold_for};
use crate::lock::with_state_lock;
use crate::lockorder::{RankedGuard, RankedMutex};
use crate::oauth;
use crate::profile::{
    AppConfig, ConfigHandle, Profile, app_state_mtime, load_config, save_app_state, save_profile,
};
use crate::update::{self, UpdateEvent};
use crate::usage::{
    ActivityKind, ActivityStore, ConsecutiveCacheHit, ConsecutiveOk, Last429At, LastFetchedAt,
    LastRotatedWindow, LearnedIntervals, NextRefreshPerProfile, OpResult, OpResultReceiver,
    OpResultSender, PendingAutoStart, PendingSwitch, PendingSwitchOff, PendingWindowRotation,
    ProfileActivity, RefetchQueue, SERVER_CACHE_TTL_ESTIMATE_MS, StartupReceiver, StartupSender,
    StartupSignal, StatusStore, TokenEntry, TokenList, UsageStore, any_busy, clear_activity,
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

/// One interactive line in the Fallback tab's detail pane when a chain member
/// is selected. Mirrors [`ConfigRow`] — built per member by [`fallback_rows`].
/// `Threshold` is a stepper (±5 on `+` / `-`); `Remove` arms then confirms,
/// the same inline arm-to-confirm pattern as the Config delete row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackRow {
    Threshold,
    /// Chain-global wrap-off toggle. Rendered as a button on every member card
    /// (the setting is per-chain, not per-member); ⏎ flips it.
    WrapOff,
    Remove,
}

/// One editable line in the Config tab's detail pane. The row set is built per
/// selection by [`config_rows`]: auto-start shows only for OAuth accounts, and
/// the trailing row swaps between `delete` (an existing account) and `create`
/// (the `+ new` draft). `Name` / `BaseUrl` / `ApiKey` are text rows; the rest
/// are single-press toggles or actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigRow {
    Name,
    BaseUrl,
    ApiKey,
    AutoStart,
    Delete,
    Create,
}

impl ConfigRow {
    /// Text rows capture keystrokes when activated; the rest act on ⏎.
    pub(crate) fn is_text(self) -> bool {
        matches!(
            self,
            ConfigRow::Name | ConfigRow::BaseUrl | ConfigRow::ApiKey
        )
    }
}

/// Inline editor state for the Config detail pane — the replacement for the old
/// new / edit / rename popups. One draft is built when focus drops into the
/// detail pane (from a profile row or the `+ new` row) and torn down when focus
/// returns to the account list.
#[derive(Debug, Clone)]
pub(crate) struct ConfigDraft {
    /// `None` while creating a new account; `Some(name)` while editing an
    /// existing one. Existing-account text edits commit per-field on ⏎; the new
    /// draft buffers all three fields until the `create` row fires.
    pub(crate) editing_name: Option<String>,
    pub(crate) name: InputState,
    pub(crate) base_url: InputState,
    pub(crate) api_key: InputState,
    /// `Some(row)` while a text row owns the keyboard (caret visible).
    pub(crate) active: Option<ConfigRow>,
    /// First ⏎ on the delete row arms it; the second confirms. Any cursor move
    /// disarms — an inline stand-in for the old delete-confirm popup.
    pub(crate) armed_delete: bool,
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
pub(crate) enum Modal {
    Confirm(ConfirmState),
    /// Credential divergence prompt — Overwrite / NewProfile / Discard.
    Divergence(DivergenceForm),
    CaptureName(CaptureNameForm),
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

// ── Tabs ──────────────────────────────────────────────────────────────────────

/// Top-level views, switched by ⇥ / ⇧⇥ / ← → and shown in the tab bar. Every
/// tab shares the same background workers; only the body and keymap change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    /// Accounts table + at-a-glance usage; the home view.
    Overview,
    /// Per-account usage breakdown (all windows, reset timers, extra credits).
    Usage,
    /// Per-account settings: endpoint, rename, auto-start, chain membership,
    /// delete. Master-detail: a profile list plus an actions pane.
    Config,
    /// Fallback chain editor — ordering and per-member thresholds.
    Fallback,
}

impl Tab {
    pub(crate) const ALL: [Tab; 4] = [Tab::Overview, Tab::Usage, Tab::Config, Tab::Fallback];

    pub(crate) fn title(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Usage => "Usage",
            Tab::Config => "Config",
            Tab::Fallback => "Fallback",
        }
    }

    pub(crate) fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    pub(crate) fn next(self) -> Tab {
        Tab::ALL[(self.index() + 1) % Tab::ALL.len()]
    }

    pub(crate) fn prev(self) -> Tab {
        Tab::ALL[(self.index() + Tab::ALL.len() - 1) % Tab::ALL.len()]
    }
}

/// Which pane owns the cursor on the Config tab. `Profiles` selects which
/// account to configure; `Actions` walks that account's settings list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigFocus {
    Profiles,
    Actions,
}

/// Which Fallback pane has focus, mirroring [`ConfigFocus`]. `Chain` drives the
/// ordered list on the left (↑↓ moves, ⇧↑↓ reorders, ⏎ drops in); `Detail`
/// drives the right pane — the member's threshold stepper + remove row, or the
/// add-candidate picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackFocus {
    Chain,
    Detail,
}

// ── Overview list items ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub(crate) enum MainItemKind {
    Profile(usize),
}

// ── App ───────────────────────────────────────────────────────────────────────

pub(crate) struct App {
    /// Shared mutable state — locked by the main thread on every read/write
    /// and by the background usage refresher when rotating tokens or kicking
    /// auto-start. Hold the guard only across the work that needs it; releasing
    /// before HTTP-heavy operations keeps the refresher from stalling the UI.
    pub(crate) config: ConfigHandle,

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
    /// Scheduler-posted wrap-off decision. When the whole chain is spent with
    /// no sink, the scheduler flips this true; `on_tick` drains it and turns
    /// off all accounts.
    pub(crate) pending_switch_off: PendingSwitchOff,
    pub(crate) refetch_queue: RefetchQueue,
    pub(crate) learned_intervals: LearnedIntervals,
    pub(crate) ok_count: ConsecutiveOk,
    pub(crate) cache_hit_count: ConsecutiveCacheHit,
    pub(crate) last_429: Last429At,

    pub(crate) tab: Tab,
    pub(crate) modals: Vec<Modal>,

    /// Cursor into `main_items()` on the Overview tab (profiles + action rows).
    pub(crate) main_cursor: usize,
    /// Selected profile index on the Usage tab.
    pub(crate) usage_cursor: usize,
    /// Selected profile index on the Config tab's left pane.
    pub(crate) config_cursor: usize,
    /// Which Config pane has focus; gates whether ↑↓ walks profiles or actions.
    pub(crate) config_focus: ConfigFocus,
    /// Cursor into the detail rows on the Config tab's right pane.
    pub(crate) config_action_cursor: usize,
    /// Inline editor for the Config detail pane. `Some` only while the Actions
    /// pane owns focus; built on entry, torn down on the way back to the list.
    pub(crate) config_draft: Option<ConfigDraft>,
    /// Cursor into `chain_items()` on the Fallback tab's left pane.
    pub(crate) chain_cursor: usize,
    /// Which Fallback pane has focus; gates whether ↑↓ walks the chain or the
    /// member's detail rows / add candidates.
    pub(crate) fallback_focus: FallbackFocus,
    /// Cursor into the right pane on the Fallback tab — `fallback_rows()` for a
    /// member, or the candidate list for the `+ add` row.
    pub(crate) fallback_detail_cursor: usize,
    /// First ⏎ on the remove row arms it; the second confirms. Any cursor move
    /// or focus change disarms — the inline stand-in for the old remove popup.
    pub(crate) fallback_armed_remove: bool,
    /// `Some` while the threshold row is being typed into (⏎ on the row opens
    /// it). Owns the keyboard like a Config text edit; `+` / `-` still step the
    /// value when this is `None`.
    pub(crate) fallback_threshold_draft: Option<InputState>,

    pub(crate) toasts: VecDeque<Toast>,
    /// Outcome of the startup update check, drained in `on_tick` into a toast.
    /// Best-effort: the worker stays silent on errors and when up to date.
    pub(crate) update_results: std::sync::mpsc::Receiver<UpdateEvent>,

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
        let usage_store: UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
        let usage_status: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
        let usage_tokens: TokenList = Arc::new(RankedMutex::new(collect_tokens(&config.profiles)));
        let next_refresh_per_profile: NextRefreshPerProfile =
            Arc::new(RankedMutex::new(HashMap::new()));
        let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
        let (op_sender, op_results) = std::sync::mpsc::channel::<OpResult>();
        let (startup_sender, startup_results) = std::sync::mpsc::channel::<StartupSignal>();
        let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
        let pending_auto_start: PendingAutoStart = Arc::new(RankedMutex::new(HashSet::new()));
        let pending_window_rotation: PendingWindowRotation =
            Arc::new(RankedMutex::new(HashMap::new()));
        let last_rotated_window: LastRotatedWindow = Arc::new(RankedMutex::new(HashMap::new()));
        let pending_switch: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));
        let pending_switch_off: PendingSwitchOff = Arc::new(RankedMutex::new(false));
        let refetch_queue: RefetchQueue = Arc::new(RankedMutex::new(HashSet::new()));
        // Restore AIMD state from disk so cadence survives restarts.
        let learned_intervals: LearnedIntervals =
            Arc::new(RankedMutex::new(config.state.learned_intervals_ms.clone()));
        let ok_count: ConsecutiveOk =
            Arc::new(RankedMutex::new(config.state.consecutive_ok_count.clone()));
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
        let cache_hit_count: ConsecutiveCacheHit = Arc::new(RankedMutex::new(restored_ch));
        let last_429: Last429At = Arc::new(RankedMutex::new(config.state.last_429_at.clone()));

        // Kick the best-effort update check on its own thread; its verdict lands
        // in `update_results` and is toasted from `on_tick`.
        let (update_sender, update_results) = std::sync::mpsc::channel::<UpdateEvent>();
        update::spawn(update_sender);

        Self {
            config: Arc::new(RankedMutex::new(config)),
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
            pending_switch_off,
            refetch_queue,
            learned_intervals,
            ok_count,
            cache_hit_count,
            last_429,
            tab: Tab::Overview,
            modals: Vec::new(),
            main_cursor: 0,
            usage_cursor: 0,
            config_cursor: 0,
            config_focus: ConfigFocus::Profiles,
            config_action_cursor: 0,
            fallback_focus: FallbackFocus::Chain,
            fallback_detail_cursor: 0,
            fallback_armed_remove: false,
            fallback_threshold_draft: None,
            config_draft: None,
            chain_cursor: 0,
            toasts: VecDeque::new(),
            update_results,
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
    pub(crate) fn config(&self) -> RankedGuard<'_, AppConfig> {
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
            Arc::clone(&self.pending_switch_off),
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
        match switched {
            Some(SwitchAction::To(target)) => {
                self.toast(ToastKind::Warning, format!("auto-switched to '{target}'"));
            }
            Some(SwitchAction::Off) => {
                self.refresh_tokens();
                self.toast(
                    ToastKind::Warning,
                    "all accounts spent — switched off to halt usage".to_string(),
                );
            }
            None => {}
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
        // Collect under the `config` lock and drop it *before* taking
        // `usage_tokens` — folding both into one assignment would hold `config`
        // (rank CONFIG) across the `usage_tokens.lock()` (rank TOKENS) acquire,
        // inverting the global lock order (TOKENS is outer of CONFIG).
        let tokens = collect_tokens(&self.config().profiles);
        *self
            .usage_tokens
            .lock()
            .expect("usage_tokens mutex poisoned") = tokens;
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
        (0..self.config().profiles.len())
            .map(MainItemKind::Profile)
            .collect()
    }

    /// Number of profiles; the clamp ceiling for the Usage / Config cursors.
    pub(crate) fn profile_count(&self) -> usize {
        self.config().profiles.len()
    }

    /// Name of the profile a tab cursor points at, if any.
    pub(crate) fn profile_name_at(&self, idx: usize) -> Option<String> {
        self.config().profiles.get(idx).map(|p| p.name.clone())
    }

    pub(crate) fn clamp_main_cursor(&mut self) {
        let len = self.config().profiles.len();
        self.main_cursor = self.main_cursor.min(len.saturating_sub(1));
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

    // A Config text field that's capturing keystrokes owns the keyboard, the
    // same way a modal would — otherwise typing `n`/`r`/`q` into a name would
    // fire the global shortcuts below.
    if app.tab == Tab::Config
        && app.config_focus == ConfigFocus::Actions
        && app
            .config_draft
            .as_ref()
            .is_some_and(|d| d.active.is_some())
    {
        handle_config_edit_key(app, key);
        return;
    }

    // The Fallback threshold editor captures keystrokes the same way, so typing
    // `90` into it can't trip the global `n` / `q` / `r` shortcuts below.
    if app.tab == Tab::Fallback
        && app.fallback_focus == FallbackFocus::Detail
        && app.fallback_threshold_draft.is_some()
    {
        handle_fallback_threshold_edit_key(app, key);
        return;
    }

    // Global keys, available on every tab when no modal owns input.
    match key.code {
        KeyCode::Tab | KeyCode::Right => {
            switch_tab(app, app.tab.next());
            return;
        }
        KeyCode::BackTab | KeyCode::Left => {
            switch_tab(app, app.tab.prev());
            return;
        }
        KeyCode::Char('?') => {
            app.modals.push(Modal::Help);
            return;
        }
        KeyCode::Char('r') => {
            app.manual_refresh();
            app.toast(ToastKind::Info, "refreshing usage…");
            return;
        }
        KeyCode::Char('t') => {
            app.modals.push(Modal::Confirm(ConfirmState {
                message: ROTATE_ALL_MSG.to_string(),
                detail: Some(ROTATE_ALL_DETAIL.to_string()),
                choice: false,
                on_confirm: ConfirmAction::RotateAll,
            }));
            return;
        }
        KeyCode::Char('n') => {
            start_new_account(app);
            return;
        }
        // Esc backs out of a Config / Fallback sub-focus; otherwise it quits.
        KeyCode::Esc => {
            if app.tab == Tab::Config && app.config_focus == ConfigFocus::Actions {
                app.config_focus = ConfigFocus::Profiles;
                app.config_draft = None;
            } else if app.tab == Tab::Fallback && app.fallback_focus == FallbackFocus::Detail {
                leave_fallback_detail(app);
            } else {
                app.quit = true;
            }
            return;
        }
        KeyCode::Char('q') => {
            app.quit = true;
            return;
        }
        _ => {}
    }

    match app.tab {
        Tab::Overview => handle_overview_key(app, key),
        Tab::Usage => handle_usage_key(app, key),
        Tab::Config => handle_config_key(app, key),
        Tab::Fallback => handle_fallback_key(app, key),
    }
}

/// Switch the active tab and re-seed that tab's cursor so it lands on a valid,
/// useful row (the active profile for Usage / Config; the chain top for
/// Fallback).
fn switch_tab(app: &mut App, tab: Tab) {
    app.tab = tab;
    // Changing tabs drops any in-flight inline config edit.
    app.config_draft = None;
    match tab {
        Tab::Overview => app.clamp_main_cursor(),
        Tab::Usage => app.usage_cursor = clamp_profile_cursor(app, app.usage_cursor),
        Tab::Config => {
            app.config_cursor = clamp_profile_cursor(app, app.config_cursor);
            app.config_focus = ConfigFocus::Profiles;
            app.config_action_cursor = 0;
        }
        Tab::Fallback => {
            app.chain_cursor = 0;
            app.fallback_focus = FallbackFocus::Chain;
            app.fallback_detail_cursor = 0;
            app.fallback_armed_remove = false;
            app.fallback_threshold_draft = None;
        }
    }
}

/// Clamp a profile-index cursor to the current profile count.
fn clamp_profile_cursor(app: &App, cursor: usize) -> usize {
    cursor.min(app.profile_count().saturating_sub(1))
}

fn handle_overview_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => reorder_main_cursor(app, -1),
        KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => reorder_main_cursor(app, 1),
        KeyCode::Up | KeyCode::Char('k') => step_main_cursor(app, -1),
        KeyCode::Down | KeyCode::Char('j') => step_main_cursor(app, 1),
        KeyCode::Enter => activate_main_item(app),
        _ => {}
    }
}

/// Up/down picks which account's usage to show. The pane is read-only; all
/// editing lives on the Config tab and switching on the Overview tab.
fn handle_usage_key(app: &mut App, key: KeyEvent) {
    let count = app.profile_count();
    if count == 0 {
        return;
    }
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.usage_cursor = if app.usage_cursor == 0 {
                count - 1
            } else {
                app.usage_cursor - 1
            };
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.usage_cursor = if app.usage_cursor + 1 >= count {
                0
            } else {
                app.usage_cursor + 1
            };
        }
        _ => {}
    }
}

/// Move the main cursor by one account, wrapping at both ends.
fn step_main_cursor(app: &mut App, delta: i32) {
    let len = app.config().profiles.len();
    if len == 0 {
        return;
    }
    app.main_cursor = (app.main_cursor as i32 + delta).rem_euclid(len as i32) as usize;
}

/// Ask to switch to the profile at `idx`. No-ops when already active; otherwise
/// raises the switch confirm modal. Shared by the Overview, Usage, and Config
/// tabs so the switch flow is identical everywhere.
fn request_switch_to(app: &mut App, idx: usize) {
    let cfg = app.config();
    let Some(name) = cfg.profiles.get(idx).map(|p| p.name.clone()) else {
        return;
    };
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

fn activate_main_item(app: &mut App) {
    let Some(item) = app.current_main_item() else {
        return;
    };
    match item {
        // No-op when already active; saves the round-trip through the confirm
        // modal and a redundant token refresh.
        MainItemKind::Profile(idx) => request_switch_to(app, idx),
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

/// True when `active`'s live `.credentials.json` has diverged from its stored
/// chain (an unsaved `/login`) and it isn't a first-login adoption — i.e. it
/// must be reconciled before any path that clears or relinks its live creds.
fn active_diverged_unsaved(active: &str) -> bool {
    matches!(
        classify_credentials_link(active).ok(),
        Some(LinkState::Diverged)
    ) && !is_first_login(active).unwrap_or(false)
}

/// Toast and raise the Divergence prompt for `active`. `verb` names the action
/// the user must resolve before (e.g. "switching", "switching off").
fn prompt_divergence(app: &mut App, active: String, verb: &str) {
    app.toast(
        ToastKind::Warning,
        format!("'{active}' has unsaved Claude Code credentials — resolve before {verb}"),
    );
    app.modals
        .push(Modal::Divergence(DivergenceForm { active, cursor: 0 }));
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
        && active_diverged_unsaved(&active)
    {
        clear_activity(&app.activity, name);
        prompt_divergence(app, active, "switching");
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

/// Turn off all accounts on the UI thread — the wrap-off decision drained from
/// `pending_switch_off`. Mirrors `finalize_switch`'s divergence guard: an
/// unsaved `/login` on the outgoing active is resolved first, since clearing the
/// live credentials would otherwise drop the fresh chain. No HTTP, runs inline.
fn perform_switch_off(app: &mut App) {
    let Some(active) = app.config().state.active_profile.clone() else {
        return;
    };
    if active_diverged_unsaved(&active) {
        prompt_divergence(app, active, "switching off");
        return;
    }
    let result = {
        let mut cfg = app.config();
        switch_off(&mut cfg)
    };
    match result {
        Ok(()) => {
            app.refresh_tokens();
            app.last_state_mtime = app_state_mtime();
            app.toast(
                ToastKind::Warning,
                "all accounts spent — switched off to halt usage".to_string(),
            );
        }
        Err(e) => app.toast(ToastKind::Danger, format!("switch-off failed: {e}")),
    }
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
    items
}

/// The detail rows shown for a chain member, in order: threshold stepper, the
/// chain-global wrap-off toggle, then the danger remove row.
pub(crate) const FALLBACK_ROWS: [FallbackRow; 3] = [
    FallbackRow::Threshold,
    FallbackRow::WrapOff,
    FallbackRow::Remove,
];

/// What the Fallback footer should advertise right now, derived from focus +
/// selection + edit state so it lists only keys that currently do something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackHint {
    Empty,
    ChainMember,
    ChainAdd,
    DetailThreshold,
    DetailThresholdEdit,
    DetailWrapOff,
    DetailRemove,
    DetailRemoveArmed,
    DetailAdd,
}

/// Resolve the footer hint context for the Fallback tab.
pub(crate) fn fallback_hint(app: &App) -> FallbackHint {
    if chain_items(app).is_empty() {
        return FallbackHint::Empty;
    }
    match app.fallback_focus {
        FallbackFocus::Chain => match selected_chain_member(app) {
            Some(_) => FallbackHint::ChainMember,
            None => FallbackHint::ChainAdd,
        },
        FallbackFocus::Detail => {
            if selected_chain_member(app).is_none() {
                return FallbackHint::DetailAdd;
            }
            if app.fallback_threshold_draft.is_some() {
                return FallbackHint::DetailThresholdEdit;
            }
            let cursor = app.fallback_detail_cursor.min(FALLBACK_ROWS.len() - 1);
            match FALLBACK_ROWS[cursor] {
                FallbackRow::Threshold => FallbackHint::DetailThreshold,
                FallbackRow::WrapOff => FallbackHint::DetailWrapOff,
                FallbackRow::Remove if app.fallback_armed_remove => FallbackHint::DetailRemoveArmed,
                FallbackRow::Remove => FallbackHint::DetailRemove,
            }
        }
    }
}

/// Fallback tab keymap. Left pane (`Chain`): ↑↓ walks the chain + `+ add` row,
/// ⇧↑↓ reorders a member, ⏎ drops focus into the right pane. Right pane
/// (`Detail`): the member's threshold stepper + remove row, or the add picker.
fn handle_fallback_key(app: &mut App, key: KeyEvent) {
    match app.fallback_focus {
        FallbackFocus::Chain => handle_fallback_chain_key(app, key),
        FallbackFocus::Detail => handle_fallback_detail_key(app, key),
    }
}

fn handle_fallback_chain_key(app: &mut App, key: KeyEvent) {
    let last = chain_items(app).len().saturating_sub(1);
    match key.code {
        KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => reorder_chain_member(app, -1),
        KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
            reorder_chain_member(app, 1)
        }
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
        KeyCode::Enter => enter_fallback_detail(app),
        _ => {}
    }
}

/// Flip the "when the whole chain is spent" behaviour and persist it. On =
/// switch off all accounts (stop usage); off = stay on the last account (keep
/// using it). Only matters once every member is over its threshold with no
/// 100% sink to land on.
fn toggle_wrap_off(app: &mut App) {
    let enabled = {
        let mut cfg = app.config();
        cfg.state.wrap_off = !cfg.state.wrap_off;
        let _ = save_app_state(&cfg.state);
        cfg.state.wrap_off
    };
    app.last_state_mtime = app_state_mtime();
    let msg = if enabled {
        "chain spent → switch off all accounts (stops usage)"
    } else {
        "chain spent → stay on last account (keeps using it)"
    };
    app.toast(ToastKind::Success, msg.to_string());
}

/// Right-pane keymap for a member: ↑↓ walks rows, `+` / `-` steps the threshold,
/// ⏎ / space on remove arms then confirms. Delegates to the add picker when the
/// cursor sits on the `+ add` row.
fn handle_fallback_detail_key(app: &mut App, key: KeyEvent) {
    if selected_chain_member(app).is_none() {
        handle_fallback_add_key(app, key);
        return;
    }
    let last = FALLBACK_ROWS.len() - 1;
    app.fallback_detail_cursor = app.fallback_detail_cursor.min(last);
    let on_threshold = FALLBACK_ROWS[app.fallback_detail_cursor] == FallbackRow::Threshold;
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.fallback_armed_remove = false;
            app.fallback_detail_cursor = if app.fallback_detail_cursor == 0 {
                last
            } else {
                app.fallback_detail_cursor - 1
            };
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.fallback_armed_remove = false;
            app.fallback_detail_cursor = if app.fallback_detail_cursor >= last {
                0
            } else {
                app.fallback_detail_cursor + 1
            };
        }
        KeyCode::Char('+' | '=') if on_threshold => adjust_threshold(app, 5.0),
        KeyCode::Char('-' | '_') if on_threshold => adjust_threshold(app, -5.0),
        KeyCode::Enter | KeyCode::Char(' ') => {
            run_fallback_row(app, FALLBACK_ROWS[app.fallback_detail_cursor]);
        }
        _ => {}
    }
}

/// `+ add` detail: a candidate picker. ↑↓ walks, ⏎ adds and re-homes focus.
fn handle_fallback_add_key(app: &mut App, key: KeyEvent) {
    let candidates = chain_candidates(app);
    if candidates.is_empty() {
        leave_fallback_detail(app);
        return;
    }
    let last = candidates.len() - 1;
    app.fallback_detail_cursor = app.fallback_detail_cursor.min(last);
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.fallback_detail_cursor = if app.fallback_detail_cursor == 0 {
                last
            } else {
                app.fallback_detail_cursor - 1
            };
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.fallback_detail_cursor = if app.fallback_detail_cursor >= last {
                0
            } else {
                app.fallback_detail_cursor + 1
            };
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let name = candidates[app.fallback_detail_cursor].clone();
            add_chain_candidate(app, &name);
            app.toast(ToastKind::Success, format!("added '{name}' to chain"));
            // Adding shrinks the picker; when it empties the `+ add` row is gone,
            // so land the cursor on the freshly-appended member instead.
            let remaining = chain_candidates(app);
            if remaining.is_empty() {
                leave_fallback_detail(app);
                app.chain_cursor = chain_items(app).len().saturating_sub(1);
            } else {
                app.fallback_detail_cursor = app.fallback_detail_cursor.min(remaining.len() - 1);
            }
        }
        _ => {}
    }
}

/// The chain index under the cursor, or `None` when it's on the `+ add` row.
fn selected_chain_member(app: &App) -> Option<usize> {
    match chain_items(app).get(app.chain_cursor).copied() {
        Some(ChainItemKind::Member(i)) => Some(i),
        _ => None,
    }
}

/// Profiles not yet in the chain — the pickable rows for the `+ add` detail.
pub(crate) fn chain_candidates(app: &App) -> Vec<String> {
    let cfg = app.config();
    cfg.profiles
        .iter()
        .filter(|p| !cfg.state.fallback_chain.iter().any(|c| c == &p.name))
        .map(|p| p.name.clone())
        .collect()
}

/// Drop focus into the right pane for the selected chain item. No-op on `+ add`
/// when nothing's left to add.
fn enter_fallback_detail(app: &mut App) {
    match chain_items(app).get(app.chain_cursor).copied() {
        Some(ChainItemKind::Member(_)) => {}
        Some(ChainItemKind::Add) if !chain_candidates(app).is_empty() => {}
        _ => return,
    }
    app.fallback_detail_cursor = 0;
    app.fallback_armed_remove = false;
    app.fallback_focus = FallbackFocus::Detail;
}

/// Lift focus back to the chain list, clearing any primed remove or live edit.
fn leave_fallback_detail(app: &mut App) {
    app.fallback_focus = FallbackFocus::Chain;
    app.fallback_armed_remove = false;
    app.fallback_detail_cursor = 0;
    app.fallback_threshold_draft = None;
}

/// ⇧↑↓ on the chain list: move the selected member up / down, following it with
/// the cursor. No-op on `+ add` or at a boundary. Chain index == cursor for
/// members (they precede the trailing `+ add` row), so the two move together.
fn reorder_chain_member(app: &mut App, delta: i32) {
    let Some(pos) = selected_chain_member(app) else {
        return;
    };
    let target = pos as i32 + delta;
    {
        let mut cfg = app.config.lock().expect("config mutex poisoned");
        if target < 0 || target as usize >= cfg.state.fallback_chain.len() {
            return;
        }
        cfg.state.fallback_chain.swap(pos, target as usize);
        let _ = save_app_state(&cfg.state);
    }
    app.chain_cursor = target as usize;
}

/// ⏎ / space on a member detail row: threshold opens the inline editor seeded
/// with the current value, remove arms on the first press and deletes on the
/// second.
fn run_fallback_row(app: &mut App, row: FallbackRow) {
    match row {
        FallbackRow::Threshold => {
            if let Some(current) = selected_threshold(app) {
                app.fallback_threshold_draft = Some(InputState::new(&format!("{current:.0}")));
            }
        }
        FallbackRow::WrapOff => toggle_wrap_off(app),
        FallbackRow::Remove => {
            if app.fallback_armed_remove {
                remove_chain_member(app);
            } else {
                app.fallback_armed_remove = true;
            }
        }
    }
}

/// Keystrokes while the threshold field is open. ⏎ parses + saves, ⎋ discards,
/// everything else edits the buffer.
fn handle_fallback_threshold_edit_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.fallback_threshold_draft = None,
        KeyCode::Enter => commit_threshold_edit(app),
        _ => {
            if let Some(input) = app.fallback_threshold_draft.as_mut() {
                apply_input_edit(input, key);
            }
        }
    }
}

/// Validate the typed threshold (a number in 0..=100) and persist it. On a bad
/// value, toast and keep the editor open so the user can fix it.
fn commit_threshold_edit(app: &mut App) {
    let Some(raw) = app.fallback_threshold_draft.as_ref().map(|i| i.trimmed()) else {
        return;
    };
    let value = match raw.parse::<f64>() {
        Ok(v) if (0.0..=100.0).contains(&v) => v,
        _ => {
            app.toast(
                ToastKind::Danger,
                "threshold must be a number between 0 and 100",
            );
            return;
        }
    };
    write_threshold(app, value);
    app.fallback_threshold_draft = None;
}

/// The selected member's effective threshold, or `None` on the `+ add` row.
fn selected_threshold(app: &App) -> Option<f64> {
    let pos = selected_chain_member(app)?;
    let cfg = app.config();
    let name = cfg.state.fallback_chain.get(pos)?;
    cfg.find(name).map(threshold_for)
}

/// Write an absolute threshold for the selected member and persist immediately.
fn write_threshold(app: &mut App, value: f64) {
    let Some(pos) = selected_chain_member(app) else {
        return;
    };
    let save_err = {
        let mut cfg = app.config.lock().expect("config mutex poisoned");
        let Some(name) = cfg.state.fallback_chain.get(pos).cloned() else {
            return;
        };
        match cfg.find_mut(&name) {
            Some(profile) => {
                profile.fallback_threshold = Some(value);
                save_profile(profile).err()
            }
            None => None,
        }
    };
    if let Some(e) = save_err {
        app.toast(ToastKind::Danger, format!("save failed: {e}"));
    }
}

/// Step the selected member's threshold by `delta`, clamped to 0..=100, and
/// persist immediately (matching the Config auto-start toggle's eager save).
fn adjust_threshold(app: &mut App, delta: f64) {
    if let Some(current) = selected_threshold(app) {
        write_threshold(app, (current + delta).clamp(0.0, 100.0));
    }
}

/// Add a profile to the chain, seeding the default threshold if unset, then
/// persist both the profile and the chain order.
fn add_chain_candidate(app: &mut App, name: &str) {
    let mut cfg = app.config.lock().expect("config mutex poisoned");
    if let Some(profile) = cfg.find_mut(name)
        && profile.fallback_threshold.is_none()
    {
        profile.fallback_threshold = Some(DEFAULT_THRESHOLD);
        let _ = save_profile(profile);
    }
    cfg.state.fallback_chain.push(name.to_string());
    let _ = save_app_state(&cfg.state);
}

/// Remove the selected member, persist, and return focus to the list with the
/// cursor clamped to a valid row.
fn remove_chain_member(app: &mut App) {
    let Some(pos) = selected_chain_member(app) else {
        return;
    };
    let name = {
        let mut cfg = app.config.lock().expect("config mutex poisoned");
        let Some(name) = cfg.state.fallback_chain.get(pos).cloned() else {
            return;
        };
        cfg.state.fallback_chain.retain(|n| n != &name);
        let _ = save_app_state(&cfg.state);
        name
    };
    leave_fallback_detail(app);
    let items_len = chain_items(app).len();
    if app.chain_cursor >= items_len {
        app.chain_cursor = items_len.saturating_sub(1);
    }
    app.toast(ToastKind::Info, format!("removed '{name}' from chain"));
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
        Modal::Confirm(_) => handle_confirm_key(app, key),
        Modal::Divergence(_) => handle_divergence_key(app, key),
        Modal::CaptureName(_) => handle_capture_name_key(app, key),
    }
}

/// Config tab keymap. Left pane: the account list plus a trailing `+ new` row,
/// ⏎ drops focus into the detail pane (building a [`ConfigDraft`]). Right pane:
/// ↑↓ walks the detail rows, ⏎ edits a text row inline / toggles a switch /
/// arms delete / creates. Esc (handled globally) lifts focus back to the list.
fn handle_config_key(app: &mut App, key: KeyEvent) {
    // The selector has one row per account plus the trailing `+ new` row.
    let sel_len = app.profile_count() + 1;
    app.config_cursor = app.config_cursor.min(sel_len - 1);

    match app.config_focus {
        ConfigFocus::Profiles => match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                app.config_cursor = if app.config_cursor == 0 {
                    sel_len - 1
                } else {
                    app.config_cursor - 1
                };
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.config_cursor = if app.config_cursor + 1 >= sel_len {
                    0
                } else {
                    app.config_cursor + 1
                };
            }
            KeyCode::Enter => enter_config_detail(app),
            _ => {}
        },
        ConfigFocus::Actions => {
            let rows = config_rows(app);
            if rows.is_empty() {
                app.config_focus = ConfigFocus::Profiles;
                app.config_draft = None;
                return;
            }
            let last = rows.len() - 1;
            app.config_action_cursor = app.config_action_cursor.min(last);
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    disarm_delete(app);
                    app.config_action_cursor = if app.config_action_cursor == 0 {
                        last
                    } else {
                        app.config_action_cursor - 1
                    };
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    disarm_delete(app);
                    app.config_action_cursor = if app.config_action_cursor >= last {
                        0
                    } else {
                        app.config_action_cursor + 1
                    };
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    run_config_row(app, rows[app.config_action_cursor]);
                }
                _ => {}
            }
        }
    }
}

/// Detail rows for the current selection. The `+ new` row (cursor past the last
/// account) yields the create form; an account yields its settings, with
/// auto-start present only for OAuth accounts.
pub(crate) fn config_rows(app: &App) -> Vec<ConfigRow> {
    let cfg = app.config();
    if app.config_cursor >= cfg.profiles.len() {
        return vec![
            ConfigRow::Name,
            ConfigRow::BaseUrl,
            ConfigRow::ApiKey,
            ConfigRow::Create,
        ];
    }
    let is_oauth = cfg
        .profiles
        .get(app.config_cursor)
        .map(|p| p.is_oauth())
        .unwrap_or(true);
    let mut rows = vec![ConfigRow::Name, ConfigRow::BaseUrl, ConfigRow::ApiKey];
    if is_oauth {
        rows.push(ConfigRow::AutoStart);
    }
    rows.push(ConfigRow::Delete);
    rows
}

/// Drop focus into the detail pane, seeding a draft for the current selection.
fn enter_config_detail(app: &mut App) {
    app.config_action_cursor = 0;
    if app.config_cursor >= app.profile_count() {
        app.config_draft = Some(build_draft_new());
    } else if let Some(name) = app.profile_name_at(app.config_cursor) {
        app.config_draft = Some(build_draft_existing(app, &name));
    } else {
        return;
    }
    app.config_focus = ConfigFocus::Actions;
}

/// Jump straight into the `+ new` create form from anywhere (the global `n`).
fn start_new_account(app: &mut App) {
    switch_tab(app, Tab::Config);
    app.config_cursor = app.profile_count();
    app.config_action_cursor = 0;
    app.config_draft = Some(build_draft_new());
    app.config_focus = ConfigFocus::Actions;
}

fn build_draft_new() -> ConfigDraft {
    ConfigDraft {
        editing_name: None,
        name: InputState::new(""),
        base_url: InputState::new(""),
        api_key: InputState::new(""),
        active: None,
        armed_delete: false,
    }
}

fn build_draft_existing(app: &App, name: &str) -> ConfigDraft {
    let cfg = app.config();
    let profile = cfg.find(name);
    ConfigDraft {
        editing_name: Some(name.to_string()),
        name: InputState::new(name),
        base_url: InputState::new(profile.and_then(|p| p.base_url.as_deref()).unwrap_or("")),
        api_key: InputState::new(profile.and_then(|p| p.api_key.as_deref()).unwrap_or("")),
        active: None,
        armed_delete: false,
    }
}

/// Cancel a primed delete the moment the cursor moves off the row.
fn disarm_delete(app: &mut App) {
    if let Some(d) = app.config_draft.as_mut() {
        d.armed_delete = false;
    }
}

/// ⏎ / space on a detail row: text rows start capturing keystrokes, toggles
/// flip in place, delete arms then confirms, create commits the draft.
fn run_config_row(app: &mut App, row: ConfigRow) {
    if row.is_text() {
        if let Some(d) = app.config_draft.as_mut() {
            d.active = Some(row);
            match row {
                ConfigRow::Name => d.name.end(),
                ConfigRow::BaseUrl => d.base_url.end(),
                ConfigRow::ApiKey => d.api_key.end(),
                _ => {}
            }
        }
        return;
    }
    let name = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone());
    match row {
        ConfigRow::AutoStart => {
            if let Some(name) = name {
                toggle_auto_start(app, &name);
            }
        }
        ConfigRow::Delete => {
            let armed = app
                .config_draft
                .as_ref()
                .map(|d| d.armed_delete)
                .unwrap_or(false);
            match (armed, name) {
                (true, Some(name)) => perform_delete(app, &name),
                _ => disarm_delete_inverse(app),
            }
        }
        ConfigRow::Create => commit_new_account(app),
        _ => {}
    }
}

/// Arm the delete row (first ⏎). Split out so `run_config_row` reads cleanly.
fn disarm_delete_inverse(app: &mut App) {
    if let Some(d) = app.config_draft.as_mut() {
        d.armed_delete = true;
    }
}

/// Keystrokes while a text row is active. ⏎ commits, ⎋ reverts, else edits.
fn handle_config_edit_key(app: &mut App, key: KeyEvent) {
    let Some(active) = app.config_draft.as_ref().and_then(|d| d.active) else {
        return;
    };
    match key.code {
        KeyCode::Esc => cancel_config_edit(app, active),
        KeyCode::Enter => commit_config_field(app, active),
        _ => {
            if let Some(d) = app.config_draft.as_mut() {
                let input = match active {
                    ConfigRow::Name => &mut d.name,
                    ConfigRow::BaseUrl => &mut d.base_url,
                    ConfigRow::ApiKey => &mut d.api_key,
                    _ => return,
                };
                apply_input_edit(input, key);
            }
        }
    }
}

/// ⎋ inside a field: existing accounts revert the buffer from the live profile;
/// the new draft simply keeps what's been typed. Either way, editing ends.
fn cancel_config_edit(app: &mut App, field: ConfigRow) {
    let editing_name = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone());
    if let Some(name) = editing_name {
        let value = {
            let cfg = app.config();
            let profile = cfg.find(&name);
            match field {
                ConfigRow::Name => name.clone(),
                ConfigRow::BaseUrl => profile.and_then(|p| p.base_url.clone()).unwrap_or_default(),
                ConfigRow::ApiKey => profile.and_then(|p| p.api_key.clone()).unwrap_or_default(),
                _ => String::new(),
            }
        };
        if let Some(d) = app.config_draft.as_mut() {
            match field {
                ConfigRow::Name => d.name = InputState::new(&value),
                ConfigRow::BaseUrl => d.base_url = InputState::new(&value),
                ConfigRow::ApiKey => d.api_key = InputState::new(&value),
                _ => {}
            }
        }
    }
    if let Some(d) = app.config_draft.as_mut() {
        d.active = None;
    }
}

/// ⏎ inside a field. The new draft buffers fields until `create`; existing
/// accounts persist per field (name → rename, url/key → endpoint).
fn commit_config_field(app: &mut App, field: ConfigRow) {
    let is_new = app
        .config_draft
        .as_ref()
        .map(|d| d.editing_name.is_none())
        .unwrap_or(true);
    if is_new {
        if let Some(d) = app.config_draft.as_mut() {
            d.active = None;
        }
        return;
    }
    match field {
        ConfigRow::Name => commit_rename(app),
        ConfigRow::BaseUrl | ConfigRow::ApiKey => commit_endpoint(app),
        _ => {
            if let Some(d) = app.config_draft.as_mut() {
                d.active = None;
            }
        }
    }
}

fn commit_rename(app: &mut App) {
    let Some(d) = app.config_draft.as_ref() else {
        return;
    };
    let Some(old) = d.editing_name.clone() else {
        return;
    };
    let new = d.name.trimmed().to_string();
    if new == old {
        if let Some(d) = app.config_draft.as_mut() {
            d.active = None;
        }
        return;
    }
    let validation = {
        let cfg = app.config();
        validate_profile_name(&new, &cfg.names(), Some(old.as_str()))
    };
    if let Err(e) = validation {
        // Keep the field in edit mode so the user can fix the name in place.
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
            if let Some(d) = app.config_draft.as_mut() {
                d.editing_name = Some(new.clone());
                d.name = InputState::new(&new);
                d.active = None;
            }
            app.toast(ToastKind::Success, format!("renamed '{old}' → '{new}'"));
        }
        Err(e) => app.toast(ToastKind::Danger, format!("rename failed: {e}")),
    }
}

fn commit_endpoint(app: &mut App) {
    let Some(d) = app.config_draft.as_ref() else {
        return;
    };
    let Some(name) = d.editing_name.clone() else {
        return;
    };
    let base_url = d.base_url.trimmed_some();
    // An API key only makes sense with a base URL; drop it for OAuth.
    let api_key = if base_url.is_some() {
        d.api_key.trimmed_some()
    } else {
        None
    };
    let result = {
        let mut cfg = app.config();
        edit_profile_endpoint(&mut cfg, &name, base_url, api_key)
    };
    match result {
        Ok(()) => {
            // Reseed from the saved profile — the API key may have been dropped.
            let (base, key) = {
                let cfg = app.config();
                let p = cfg.find(&name);
                (
                    p.and_then(|p| p.base_url.clone()).unwrap_or_default(),
                    p.and_then(|p| p.api_key.clone()).unwrap_or_default(),
                )
            };
            if let Some(d) = app.config_draft.as_mut() {
                d.base_url = InputState::new(&base);
                d.api_key = InputState::new(&key);
                d.active = None;
            }
            app.toast(ToastKind::Success, format!("updated '{name}'"));
        }
        Err(e) => app.toast(ToastKind::Danger, format!("edit failed: {e}")),
    }
}

fn commit_new_account(app: &mut App) {
    let Some(d) = app.config_draft.as_ref() else {
        return;
    };
    let name = d.name.trimmed().to_string();
    let base_url = d.base_url.trimmed_some();
    let api_key = d.api_key.trimmed_some();
    let validation = {
        let cfg = app.config();
        validate_profile_name(&name, &cfg.names(), None)
    };
    if let Err(e) = validation {
        app.toast(ToastKind::Danger, format!("{e}"));
        return;
    }
    // API key only applies to endpoint profiles.
    let api_key = if base_url.is_some() { api_key } else { None };
    let result = {
        let mut cfg = app.config();
        create_blank_profile(&mut cfg, name.clone(), base_url, api_key)
    };
    match result {
        Ok(()) => {
            app.refresh_tokens();
            app.last_state_mtime = app_state_mtime();
            // Land the cursor on the freshly created account.
            let new_idx = app
                .config()
                .profiles
                .iter()
                .position(|p| p.name == name)
                .unwrap_or(0);
            app.config_cursor = new_idx;
            app.config_focus = ConfigFocus::Profiles;
            app.config_draft = None;
            app.toast(ToastKind::Success, format!("created '{name}'"));
        }
        Err(e) => app.toast(ToastKind::Danger, format!("create failed: {e}")),
    }
}

fn perform_delete(app: &mut App, name: &str) {
    let result = {
        let mut cfg = app.config();
        delete_profile(&mut cfg, name)
    };
    match result {
        Ok(()) => {
            app.refresh_tokens();
            app.last_state_mtime = app_state_mtime();
            app.config_focus = ConfigFocus::Profiles;
            app.config_draft = None;
            app.config_cursor = app.config_cursor.min(app.profile_count().saturating_sub(1));
            app.clamp_main_cursor();
            app.toast(ToastKind::Success, format!("deleted '{name}'"));
        }
        Err(e) => app.toast(ToastKind::Danger, format!("delete failed: {e}")),
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

    // Surface the background update check. `try_recv` is non-blocking, and the
    // worker emits at most one event, so this drains cheaply every tick.
    while let Ok(ev) = app.update_results.try_recv() {
        match ev {
            UpdateEvent::Installed(v) => {
                app.toast(
                    ToastKind::Success,
                    format!("updated to v{v} — restart to apply"),
                );
            }
            UpdateEvent::Available(v) => {
                app.toast(
                    ToastKind::Info,
                    format!("update available: v{v} — run `cargo install clauth`"),
                );
            }
        }
    }

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

    // Drain the scheduler's wrap-off decision: the whole chain is spent with no
    // sink, so turn off all accounts. The bool collapses repeated sets. Only
    // drain when no modal is open — `perform_switch_off` may raise a Divergence
    // prompt, and consuming the flag while one is already up would let the
    // scheduler re-set it and stack duplicate modals. Left set, it retries once
    // the modal closes.
    if app.modals.is_empty() {
        let switch_off_pending = app
            .pending_switch_off
            .lock()
            .map(|mut g| std::mem::replace(&mut *g, false))
            .unwrap_or(false);
        if switch_off_pending {
            perform_switch_off(app);
        }
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
    use crate::lockorder::RankedMutex;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::usage::{ActivityStore, ProfileActivity, any_busy};

    fn make_activity(entries: &[(&str, ProfileActivity)]) -> ActivityStore {
        let mut map = HashMap::new();
        for (name, activity) in entries {
            map.insert(name.to_string(), *activity);
        }
        Arc::new(RankedMutex::new(map))
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
