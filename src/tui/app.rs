//! Application state, keymap, and tick logic.
//!
//! Layout invariants:
//!   - Overview: read-only account list; `profile_cursor` is shared with Usage
//!     and Config so the highlight follows across tab switches.
//!   - Config: master-detail — account list + `+ new` row + inline editor
//!     (`config_draft`). No popups for create / edit / rename / delete.
//!   - Fallback: master-detail — ordered chain + `+ add` on the left; inline
//!     threshold stepper / remove / add-picker on the right. No popups.
//!   - Modals stack: top of `modals` owns input; events reach the screen below
//!     only when the stack is empty.

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
    AppConfig, ConfigHandle, Profile, ThemeName, app_state_mtime, load_config, save_app_state,
    save_profile,
};
use crate::runtime::has_live_session;
use crate::status::{self, Incident, StatusEvent};
use crate::tui::theme;
use crate::update::{self, UpdateEvent};
use crate::usage::{
    ActivityKind, ActivityStore, LastFetchedAt, NextRefreshPerProfile, OpResult, OpResultReceiver,
    OpResultSender, PendingAutoStart, PendingSwitch, PendingSwitchOff, ProfileActivity,
    RefetchQueue, StartupReceiver, StartupSender, StartupSignal, StatusStore, ThirdPartyList,
    ThirdPartyStatusStore, ThirdPartyUsageStore, TokenEntry, TokenList, UsageStore, any_busy,
    clear_activity, collect_third_party_entries, fetch_all_into, is_idle, mark_activity, now_ms,
    spawn_refresher,
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

    /// Delete the word (run of non-spaces, plus any trailing spaces) left of the
    /// caret — the `ctrl+w` editor verb.
    pub(crate) fn delete_word(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let head = &self.value[..self.cursor];
        let trimmed = head.trim_end_matches(' ');
        let start = trimmed.rfind(' ').map(|i| i + 1).unwrap_or(0);
        self.value.replace_range(start..self.cursor, "");
        self.cursor = start;
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

/// One interactive line in the Fallback tab's detail pane for a chain member.
/// `Threshold` is a stepper (±5 on `+`/`-`); `Remove` arms then confirms. The
/// chain-global wrap-off setting lives on the program-wide Config tab, not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackRow {
    Threshold,
    Remove,
}

/// One editable line in the Setup tab's detail pane. Built per selection by
/// [`config_rows`]: auto-start only for OAuth; trailing row is `delete` or
/// `create`. `Name`/`BaseUrl`/`ApiKey` are text rows; the rest are toggles/actions.
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
    /// Text rows capture keystrokes; the rest act on ⏎.
    pub(crate) fn is_text(self) -> bool {
        matches!(
            self,
            ConfigRow::Name | ConfigRow::BaseUrl | ConfigRow::ApiKey
        )
    }
}

/// One row on the program-wide Config tab. These back real persisted globals in
/// [`AppState`] — no decorative toggles. ⏎/space cycles or flips in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GlobalConfigRow {
    /// Color-depth tier: `full` (truecolor) / `compatible` (xterm-256).
    /// Persists to `[theme]` and live-swaps the active palette.
    Theme,
    /// Chain-wide "when spent" behavior (`AppState.wrap_off`) — surfaced here as
    /// a program-wide default alongside the Fallback detail row.
    WrapOff,
}

/// Inline editor state for the Config detail pane. Built on entry, torn down
/// when focus returns to the account list.
#[derive(Debug, Clone)]
pub(crate) struct ConfigDraft {
    /// `None` while creating; `Some(name)` while editing. Existing accounts
    /// commit per-field on ⏎; new drafts buffer until the `create` row fires.
    pub(crate) editing_name: Option<String>,
    pub(crate) name: InputState,
    pub(crate) base_url: InputState,
    pub(crate) api_key: InputState,
    /// `Some(row)` while a text row owns the keyboard.
    pub(crate) active: Option<ConfigRow>,
    /// First ⏎ on delete arms it; second confirms. Any cursor move disarms.
    pub(crate) armed_delete: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ConfirmState {
    pub(crate) message: String,
    pub(crate) detail: Option<String>,
    /// Highlighted choice: false = cancel, true = confirm.
    pub(crate) choice: bool,
    pub(crate) on_confirm: ConfirmAction,
}

#[derive(Debug, Clone)]
pub(crate) enum ConfirmAction {
    /// `bool` = `from_divergence`, carried through for deferred-detach semantics.
    CaptureConflict(Box<CaptureSnapshot>, bool),
    Switch(String),
    /// Confirm before discarding CC's freshly-written credentials and relinking.
    DiscardDivergence(String),
    /// Force-rotate all refresh tokens; active sessions may be logged out.
    RotateAll,
    /// Force-rotate one account's refresh token (action-menu "rotate tokens" on
    /// the focused account).
    RotateOne(String),
}

#[derive(Debug, Clone)]
pub(crate) struct CaptureNameForm {
    pub(crate) snapshot: Box<CaptureSnapshot>,
    pub(crate) input: InputState,
    /// Set when initiated by `NewProfile` divergence. Detach + deactivate of
    /// the prior profile is deferred to the success arm so cancel leaves it intact.
    pub(crate) from_divergence: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DivergenceChoice {
    Overwrite,
    NewProfile,
    Discard,
}

/// Credential-divergence prompt. Shown at startup and on the 1Hz poll when
/// `.credentials.json` no longer matches the active profile's stored creds.
/// Three actions: Overwrite, NewProfile, or Discard.
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

/// One entry in the action menu.
#[derive(Debug, Clone)]
pub(crate) struct ActionItem {
    pub(crate) label: &'static str,
    /// Single-letter shortcut auto-assigned by the hotkey algorithm; `None` when
    /// all candidate characters were already taken.
    pub(crate) hotkey: Option<char>,
    /// The logical action to fire when this item is selected.
    pub(crate) action: ActionMenuAction,
}

/// Logical actions the action menu can dispatch. Each variant maps 1-to-1 to
/// an existing key handler so there is no duplicated dispatch logic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActionMenuAction {
    // Global
    NewAccount,
    RefreshUsage,
    RotateTokens,
    // Overview
    SwitchToSelected,
    // Config
    ConfigureSelected,
    // Fallback chain
    OpenChainMember,
    ReorderUp,
    ReorderDown,
    // Fallback detail
    EditThreshold,
    ToggleWrapOff,
    RemoveMember,
    // Config detail actions (proxied through run_config_row)
    ToggleAutoStart,
    DeleteProfile,
    CreateProfile,
    EditField,
    // Program-wide Config tab
    CycleTheme,
    // Status tab
    RefreshStatus,
    OpenIncidentLink,
}

/// State for the action-menu modal.
#[derive(Debug, Clone)]
pub(crate) struct ActionMenuState {
    pub(crate) items: Vec<ActionItem>,
    pub(crate) cursor: usize,
}

impl ActionMenuState {
    /// Build and assign hotkeys in source order per the SKILL.md algorithm.
    /// Reserved keys: `a` `x` `?` `q`.
    pub(crate) fn new(actions: Vec<ActionMenuAction>) -> Self {
        const RESERVED: &[char] = &['a', 'x', '?', 'q'];
        let mut claimed: Vec<char> = Vec::new();
        let items = actions
            .into_iter()
            .map(|action| {
                let label = action.label();
                // An explicit override wins when free; else scan the first 3 alpha
                // chars of the label per the SKILL.md algorithm.
                let hotkey = action
                    .preferred_hotkey()
                    .filter(|c| !RESERVED.contains(c) && !claimed.contains(c))
                    .or_else(|| {
                        label
                            .chars()
                            .filter(|c| c.is_alphabetic())
                            .map(|c| c.to_lowercase().next().unwrap_or(c))
                            .take(3)
                            .find(|c| !RESERVED.contains(c) && !claimed.contains(c))
                    })
                    .inspect(|c| claimed.push(*c));
                ActionItem {
                    label,
                    hotkey,
                    action,
                }
            })
            .collect();
        Self { items, cursor: 0 }
    }
}

impl ActionMenuAction {
    /// Explicit hotkey override, taking priority over the label scan when free.
    /// `rotate tokens` pins `t` (mnemonic + matches the global rotate key) instead
    /// of falling to `o` after `refresh usage` claims `r`.
    fn preferred_hotkey(&self) -> Option<char> {
        match self {
            Self::RotateTokens => Some('t'),
            _ => None,
        }
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::NewAccount => "new account",
            Self::RefreshUsage => "refresh usage",
            Self::RotateTokens => "rotate tokens",
            Self::SwitchToSelected => "switch to selected",
            Self::ConfigureSelected => "configure",
            Self::OpenChainMember => "open",
            Self::ReorderUp => "reorder up",
            Self::ReorderDown => "reorder down",
            Self::EditThreshold => "edit threshold",
            Self::ToggleWrapOff => "toggle wrap-off",
            Self::RemoveMember => "remove member",
            Self::ToggleAutoStart => "toggle auto-start",
            Self::DeleteProfile => "delete profile",
            Self::CreateProfile => "create profile",
            Self::EditField => "edit field",
            Self::CycleTheme => "cycle theme",
            Self::RefreshStatus => "refresh status",
            Self::OpenIncidentLink => "open in browser",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Modal {
    Confirm(ConfirmState),
    /// Credential divergence prompt.
    Divergence(DivergenceForm),
    CaptureName(CaptureNameForm),
    Help,
    /// Context-sensitive action menu opened by `a`.
    ActionMenu(ActionMenuState),
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

const ROTATE_ALL_MSG: &str = "rotate tokens for all accounts?";
const ROTATE_ALL_DETAIL: &str = "accounts with a live session might be logged out.";
const ROTATE_ONE_DETAIL: &str = "a live session on this account might be logged out.";
const TOAST_CAPACITY: usize = 3;
const TOAST_TTL_NORMAL: Duration = Duration::from_secs(3);
const TOAST_TTL_DANGER: Duration = Duration::from_secs(6);

// ── Tabs ──────────────────────────────────────────────────────────────────────

/// Top-level views (⇥/⇧⇥/←→). All tabs share background workers; only the
/// body and keymap differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    /// Accounts table + at-a-glance usage.
    Overview,
    /// Per-account usage breakdown.
    Usage,
    /// Per-account settings (endpoint, rename, auto-start, delete).
    Setup,
    /// Fallback chain editor — ordering and per-member thresholds.
    Fallback,
    /// Program-wide settings: theme tier and global defaults.
    Config,
    /// Claude service status feed (incidents from status.claude.com).
    Status,
}

impl Tab {
    pub(crate) const ALL: [Tab; 6] = [
        Tab::Overview,
        Tab::Usage,
        Tab::Setup,
        Tab::Fallback,
        Tab::Config,
        Tab::Status,
    ];

    pub(crate) fn title(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Usage => "Usage",
            Tab::Setup => "Setup",
            Tab::Fallback => "Fallback",
            Tab::Config => "Config",
            Tab::Status => "Status",
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

/// Which Config pane owns the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigFocus {
    Profiles,
    Actions,
}

/// Which Fallback pane has focus. `Chain`: ordered list (↑↓, ⇧↑↓ reorders, ⏎
/// enters). `Detail`: threshold stepper + remove row, or add-candidate picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackFocus {
    Chain,
    Detail,
}

// ── Status tab ─────────────────────────────────────────────────────────────────

/// Which Status pane has focus. `List`: the incident selector (↑↓ + ⏎ descends).
/// `Detail`: the selected incident's timeline (↑↓ scrolls).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusFocus {
    List,
    Detail,
}

/// UI-thread-only state for the Status tab. Fed by [`StatusEvent`]s drained in
/// `on_tick`; never shared with the background thread (which only sends events).
#[derive(Debug)]
pub(crate) struct StatusState {
    /// Incidents in feed order (newest first).
    pub(crate) incidents: Vec<Incident>,
    /// Wall-clock ms of the last successful fetch / cache load; `None` until any
    /// event arrives. Drives the "cached / stale" age cue.
    pub(crate) fetched_at_ms: Option<u64>,
    /// True when the last event was `Cached` (startup cache or fetch-failed
    /// fallback) — render the data as cached rather than fresh.
    pub(crate) cached: bool,
    /// Set only when a fetch failed with nothing cached to show.
    pub(crate) error: Option<String>,
    /// A manual refresh is in flight → the panel title shows a spinner.
    pub(crate) fetching: bool,
    /// Selected incident index in `incidents`.
    pub(crate) cursor: usize,
    pub(crate) focus: StatusFocus,
    /// Scroll offset (lines) into the detail timeline.
    pub(crate) detail_scroll: u16,
    /// Max valid `detail_scroll` from the last detail render (`total - viewport`).
    /// The render pass owns the real geometry, so it writes the bound here (via
    /// `&App` interior mutability) and the key handler clamps against it — that
    /// stops a held ↓ from inflating `detail_scroll` toward `u16::MAX`.
    pub(crate) detail_max_scroll: std::cell::Cell<u16>,
    /// Newest incident id already signalled, so a refresh only toasts genuinely
    /// new incidents (not the initial load).
    pub(crate) seen_latest: Option<String>,
}

impl Default for StatusState {
    fn default() -> Self {
        Self {
            incidents: Vec::new(),
            fetched_at_ms: None,
            cached: false,
            error: None,
            fetching: false,
            cursor: 0,
            focus: StatusFocus::List,
            detail_scroll: 0,
            detail_max_scroll: std::cell::Cell::new(0),
            seen_latest: None,
        }
    }
}

impl StatusState {
    /// The incident currently under the cursor, if any.
    pub(crate) fn selected(&self) -> Option<&Incident> {
        self.incidents.get(self.cursor)
    }

    /// How many incidents are still active (status not `resolved`/`completed`).
    pub(crate) fn active_count(&self) -> usize {
        self.incidents.iter().filter(|i| i.is_active()).count()
    }
}

/// An incident is active when its lifecycle status is not terminal
/// (`resolved` / `completed`). Thin wrapper over [`Incident::is_active`] for the
/// render layer.
pub(crate) fn incident_is_active(incident: &Incident) -> bool {
    incident.is_active()
}

// ── Footer alert ─────────────────────────────────────────────────────────────

/// A transient message that replaces the hint bar in place until dismissed.
/// `x` clears it (after all toasts are gone — see dismissal precedence).
/// Add variants as new conditions arise; keep one active at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FooterAlert {
    /// `! <message>` in `WARNING` color — user must act or acknowledge.
    Warn(String),
}

// ── Banner ────────────────────────────────────────────────────────────────────

/// Severity for the full-width body banner. Ordering matters: `Danger`
/// outranks `Warning`, so when two conditions could both be live the
/// higher-severity banner wins (see `update_banner`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BannerSeverity {
    Warning,
    Danger,
}

/// A sticky system-wide condition rendered as a full-width row at the top of
/// the body (below the header, above the screen content). Computed from app
/// state each frame; not stored as a notification and not user-dismissable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Banner {
    pub(crate) severity: BannerSeverity,
    pub(crate) message: String,
}

// ── Overview list items ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub(crate) enum MainItemKind {
    Profile(usize),
}

// ── App ───────────────────────────────────────────────────────────────────────

pub(crate) struct App {
    /// Shared config; locked by the UI thread and the background refresher.
    /// Release before HTTP to avoid stalling the UI.
    pub(crate) config: ConfigHandle,

    pub(crate) usage_store: UsageStore,
    pub(crate) usage_status: StatusStore,
    pub(crate) usage_tokens: TokenList,
    pub(crate) next_refresh_per_profile: NextRefreshPerProfile,
    /// Per-profile activity state; read by the render loop for spinners, written
    /// by workers. Never held across HTTP.
    pub(crate) activity: ActivityStore,
    /// Drained in `on_tick`; each result clears the activity slot and surfaces errors.
    pub(crate) op_results: OpResultReceiver,
    /// Sender side; cloned into workers so they can report without holding a lock.
    pub(crate) op_sender: OpResultSender,
    /// Startup signals from reconcile/bootstrap workers; drained in `on_tick`.
    pub(crate) startup_results: StartupReceiver,
    /// Sender side for startup workers.
    pub(crate) startup_sender: StartupSender,
    pub(crate) last_fetched: LastFetchedAt,
    pub(crate) pending_auto_start: PendingAutoStart,
    /// name → epoch-ms first seen windowless; debounces live-session candidates.
    /// Pruned each tick to the current candidate set.
    pub(crate) auto_start_windowless_since: HashMap<String, u64>,
    /// Scheduler-posted auto-switch decisions; drained in `on_tick`.
    pub(crate) pending_switch: PendingSwitch,
    /// Set by the scheduler when the whole chain is spent; `on_tick` drains it
    /// and turns off all accounts.
    pub(crate) pending_switch_off: PendingSwitchOff,
    pub(crate) refetch_queue: RefetchQueue,

    pub(crate) third_party_tokens: ThirdPartyList,
    pub(crate) third_party_usage_store: ThirdPartyUsageStore,
    pub(crate) third_party_status: ThirdPartyStatusStore,
    pub(crate) tab: Tab,
    pub(crate) modals: Vec<Modal>,

    /// Selected account index, shared across Overview/Usage/Setup tabs.
    /// On Setup may also rest on the trailing `+ new` row (== profile_count).
    pub(crate) profile_cursor: usize,
    /// Which Setup pane has focus.
    pub(crate) config_focus: ConfigFocus,
    /// Cursor into the detail rows on the Setup tab's right pane.
    pub(crate) config_action_cursor: usize,
    /// Inline editor for the Config detail pane; `Some` only while Actions has focus.
    pub(crate) config_draft: Option<ConfigDraft>,
    /// Cursor into `chain_items()` on the Fallback left pane.
    pub(crate) chain_cursor: usize,
    /// Which Fallback pane has focus.
    pub(crate) fallback_focus: FallbackFocus,
    /// Cursor into the Fallback right pane (member rows or add-candidate list).
    pub(crate) fallback_detail_cursor: usize,
    /// First ⏎ on remove arms it; second confirms. Cursor move or focus change disarms.
    pub(crate) fallback_armed_remove: bool,
    /// `Some` while the threshold field is open (⏎ opens, owns keyboard).
    /// `+`/`-` still step the value when `None`.
    pub(crate) fallback_threshold_draft: Option<InputState>,
    /// Cursor into [`GLOBAL_CONFIG_ROWS`] on the program-wide Config tab.
    pub(crate) global_config_cursor: usize,

    pub(crate) toasts: VecDeque<Toast>,
    /// Whether the terminal is currently too short for the normal layout (< 14 rows).
    /// Tracked across frames so the "too small" toast fires only on the transition in.
    pub(crate) compact: bool,
    /// Startup update check result; drained in `on_tick`. Silent on errors.
    pub(crate) update_results: std::sync::mpsc::Receiver<UpdateEvent>,

    /// Claude status feed state; UI-thread-only (no shared lock).
    pub(crate) status: StatusState,
    /// Status feed events from the background thread; drained in `on_tick`.
    pub(crate) status_events: std::sync::mpsc::Receiver<StatusEvent>,
    /// Manual-refresh signal to the status thread; a `()` triggers a refetch.
    pub(crate) status_refresh: std::sync::mpsc::Sender<()>,

    pub(crate) last_state_mtime: Option<SystemTime>,
    pub(crate) started_at: Instant,
    /// Tick counter; advances the activity spinner frame each `on_tick`.
    pub(crate) tick_count: u64,
    pub(crate) quit: bool,
    /// First `q` at top level arms this; second `q` confirms quit.
    pub(crate) armed_quit: bool,
    /// Live footer alert; replaces the hint bar in place while `Some`.
    /// Cleared on disarm, tab switch, or explicit `x` dismiss.
    pub(crate) footer_alert: Option<FooterAlert>,
    /// Active banner; recomputed each tick from sticky system conditions.
    /// `None` when no condition holds (banner row absent from layout).
    pub(crate) banner: Option<Banner>,
    /// Last time the 1Hz divergence poll ran.
    pub(crate) last_divergence_check: Instant,
    /// Set once reconcile reports back; gates bootstrap spawn.
    pub(crate) reconcile_done: bool,
    /// Set once `spawn_bootstrap` is dispatched; prevents double-dispatch.
    pub(crate) bootstrap_started: bool,
    /// Set before bootstrap spawn, cleared on every worker exit path.
    /// `ConfirmAction::RotateAll` checks this alongside `any_busy` to block a
    /// concurrent rotate-all from racing the bootstrap's relink + initial fetch.
    pub(crate) bootstrap_active: Arc<AtomicBool>,
    /// Per-tab background-event indicator; `None` = no pending activity.
    /// Set when a background event fires on a tab that isn't currently active;
    /// cleared in `switch_tab` when the user visits that tab.
    pub(crate) tab_activity: [Option<ToastKind>; Tab::ALL.len()],
}

/// Cloned `Arc`s bundled for [`spawn_refresher`]; carries no lock rank and is
/// safe to construct while holding any lock.
struct WorkerHandles {
    config: ConfigHandle,
    usage_tokens: TokenList,
    usage_store: UsageStore,
    usage_status: StatusStore,
    next_refresh_per_profile: NextRefreshPerProfile,
    activity: ActivityStore,
    last_fetched: LastFetchedAt,
    pending_switch: PendingSwitch,
    pending_switch_off: PendingSwitchOff,
    refetch_queue: RefetchQueue,
    third_party_tokens: ThirdPartyList,
    third_party_usage_store: ThirdPartyUsageStore,
    third_party_status: ThirdPartyStatusStore,
}

impl WorkerHandles {
    /// Clone every scheduler `Arc` out of `app`.
    fn from_app(app: &App) -> Self {
        Self {
            config: Arc::clone(&app.config),
            usage_tokens: Arc::clone(&app.usage_tokens),
            usage_store: Arc::clone(&app.usage_store),
            usage_status: Arc::clone(&app.usage_status),
            next_refresh_per_profile: Arc::clone(&app.next_refresh_per_profile),
            activity: Arc::clone(&app.activity),
            last_fetched: Arc::clone(&app.last_fetched),
            pending_switch: Arc::clone(&app.pending_switch),
            pending_switch_off: Arc::clone(&app.pending_switch_off),
            refetch_queue: Arc::clone(&app.refetch_queue),
            third_party_tokens: Arc::clone(&app.third_party_tokens),
            third_party_usage_store: Arc::clone(&app.third_party_usage_store),
            third_party_status: Arc::clone(&app.third_party_status),
        }
    }
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
        let pending_switch: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));
        let pending_switch_off: PendingSwitchOff = Arc::new(RankedMutex::new(false));
        let refetch_queue: RefetchQueue = Arc::new(RankedMutex::new(HashSet::new()));
        let third_party_tokens: ThirdPartyList = Arc::new(RankedMutex::new(
            collect_third_party_entries(&config.profiles),
        ));
        let third_party_usage_store: ThirdPartyUsageStore =
            Arc::new(RankedMutex::new(HashMap::new()));
        let third_party_status: ThirdPartyStatusStore = Arc::new(RankedMutex::new(HashMap::new()));

        // Kick the best-effort update check on its own thread; its verdict lands
        // in `update_results` and is toasted from `on_tick`.
        let (update_sender, update_results) = std::sync::mpsc::channel::<UpdateEvent>();
        update::spawn(update_sender);

        // Status feed worker: streams incidents over `status_events`; a `()` on
        // `status_refresh` triggers a manual refetch. The channels are always
        // created (so the drains stay inert), but the thread is skipped under
        // `cfg!(test)`: a detached worker could outlive a test's `HOME_OVERRIDE`
        // scope and its cache write would then resolve the real `~/.clauth`.
        let (status_sender, status_events) = std::sync::mpsc::channel::<StatusEvent>();
        let (status_refresh, status_refresh_rx) = std::sync::mpsc::channel::<()>();
        if cfg!(test) {
            // Drop the worker's ends so they aren't flagged unused; the stored
            // `status_refresh` sender simply has no receiver in tests.
            drop((status_sender, status_refresh_rx));
        } else {
            status::spawn(status_sender, status_refresh_rx);
        }

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
            auto_start_windowless_since: HashMap::new(),
            pending_switch,
            pending_switch_off,
            refetch_queue,
            third_party_tokens,
            third_party_usage_store,
            third_party_status,
            tab: Tab::Overview,
            modals: Vec::new(),
            profile_cursor: 0,
            config_focus: ConfigFocus::Profiles,
            config_action_cursor: 0,
            fallback_focus: FallbackFocus::Chain,
            fallback_detail_cursor: 0,
            fallback_armed_remove: false,
            fallback_threshold_draft: None,
            global_config_cursor: 0,
            config_draft: None,
            chain_cursor: 0,
            toasts: VecDeque::new(),
            compact: false,
            update_results,
            status: StatusState::default(),
            status_events,
            status_refresh,
            last_state_mtime: app_state_mtime(),
            started_at: Instant::now(),
            tick_count: 0,
            quit: false,
            armed_quit: false,
            footer_alert: None,
            banner: None,
            last_divergence_check: Instant::now(),
            reconcile_done: false,
            bootstrap_started: false,
            bootstrap_active: Arc::new(AtomicBool::new(false)),
            tab_activity: [None; Tab::ALL.len()],
        }
    }

    /// Lock the shared AppConfig. Order: AppConfig outer, `with_state_lock` inner.
    pub(crate) fn config(&self) -> RankedGuard<'_, AppConfig> {
        self.config.lock().expect("config mutex poisoned")
    }

    /// Spawn the bootstrap on a background thread (never blocks first paint).
    /// Re-links credentials, runs initial usage fetch; no proactive token rotation
    /// (401-recovery is lazy). Posts `StartupSignal::BootstrapDone` when done;
    /// the UI thread then rebuilds the token snapshot, starts the scheduler,
    /// applies usage, and runs the startup auto-switch.
    pub(crate) fn spawn_bootstrap(&self) {
        let config = Arc::clone(&self.config);
        let usage_store = Arc::clone(&self.usage_store);
        let usage_status = Arc::clone(&self.usage_status);
        let last_fetched = Arc::clone(&self.last_fetched);
        let refetch_queue = Arc::clone(&self.refetch_queue);
        let activity = Arc::clone(&self.activity);
        let startup_sender = self.startup_sender.clone();
        let bootstrap_active = Arc::clone(&self.bootstrap_active);

        let startup_sender_for_panic = startup_sender.clone();
        let bootstrap_active_for_panic = Arc::clone(&bootstrap_active);
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Re-establish the credentials symlink (shutdown replaced it with
                // a plain file); without this, CC refreshes bypass the profile.
                let active = config
                    .lock()
                    .expect("config mutex poisoned")
                    .state
                    .active_profile
                    .clone();
                if let Some(active) = active {
                    let _ = link_profile_credentials(&active);
                }

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
                );

                // 5h windows are armed by the windowless scan in `on_tick` after
                // bootstrap clears `bootstrap_active` — no startup-only kick pass.

                bootstrap_active.store(false, Ordering::SeqCst);
                let _ = startup_sender.send(StartupSignal::BootstrapDone);
            }));
            if result.is_err() {
                // Panic path: clear flag and unblock the scheduler.
                bootstrap_active_for_panic.store(false, Ordering::SeqCst);
                let _ = startup_sender_for_panic.send(StartupSignal::BootstrapDone);
            }
        });
    }

    /// UI-thread tail of bootstrap: rebuilds token snapshot, starts scheduler,
    /// applies usage, runs startup auto-switch. No HTTP.
    fn finish_bootstrap(&mut self) {
        self.refresh_tokens();
        self.start_scheduler();
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

    /// Bundle scheduler `Arc`s and launch the background refresher.
    fn start_scheduler(&self) {
        let h = WorkerHandles::from_app(self);
        spawn_refresher(
            h.config,
            h.usage_tokens,
            h.usage_store,
            h.usage_status,
            h.next_refresh_per_profile,
            h.activity,
            h.last_fetched,
            h.pending_switch,
            h.pending_switch_off,
            h.refetch_queue,
            h.third_party_tokens,
            h.third_party_usage_store,
            h.third_party_status,
        );
    }

    pub(crate) fn apply_usage(&mut self) {
        // Poisoned lock: keep prior value rather than blanking all usage.
        // A blank map would blind auto-switch permanently.
        // Third-party stores BEFORE OAuth stores: ranks 270/280 < 300/350.
        let third_party_map = self.third_party_usage_store.lock().ok();
        let third_party_status_map = self.third_party_status.lock().ok();
        let info_map = self.usage_store.lock().ok();
        let status_map = self.usage_status.lock().ok();
        let mut cfg = self.config();
        for p in &mut cfg.profiles {
            if let Some(s) = info_map.as_ref() {
                p.usage = s.get(p.name.as_str()).cloned();
            }
            // OAuth fetch_status takes precedence; third-party only when no OAuth status.
            if let Some(s) = status_map.as_ref()
                && s.contains_key(p.name.as_str())
            {
                p.fetch_status = s.get(p.name.as_str()).copied();
            } else if let Some(s) = third_party_status_map.as_ref() {
                p.fetch_status = s.get(p.name.as_str()).copied();
            }
            if let Some(s) = third_party_map.as_ref()
                && s.contains_key(p.name.as_str())
            {
                p.third_party_usage = s.get(p.name.as_str()).cloned();
            }
        }
    }

    /// Reload config if state mtime changed. Returns true on reload.
    pub(crate) fn reload_if_state_changed(&mut self) -> bool {
        let current = app_state_mtime();
        if current == self.last_state_mtime {
            return false;
        }
        if let Ok(fresh) = load_config() {
            *self.config() = fresh;
            self.last_state_mtime = current;
            let profiles = &self.config().profiles;
            *self
                .usage_tokens
                .lock()
                .expect("usage_tokens mutex poisoned") = collect_tokens(profiles);
            *self
                .third_party_tokens
                .lock()
                .expect("third_party_tokens mutex poisoned") =
                collect_third_party_entries(profiles);
            true
        } else {
            false
        }
    }

    pub(crate) fn refresh_tokens(&self) {
        // Drop `config` lock before taking `usage_tokens` — folding them would
        // invert lock order (TOKENS is outer of CONFIG).
        let tokens = collect_tokens(&self.config().profiles);
        let third_party = collect_third_party_entries(&self.config().profiles);
        *self
            .usage_tokens
            .lock()
            .expect("usage_tokens mutex poisoned") = tokens;
        *self
            .third_party_tokens
            .lock()
            .expect("third_party_tokens mutex poisoned") = third_party;
    }

    /// Queue every profile for an immediate re-fetch (Overview `r`).
    pub(crate) fn manual_refresh(&self) {
        let names: Vec<String> = self
            .usage_tokens
            .lock()
            .expect("usage_tokens mutex poisoned")
            .iter()
            .map(|e| e.name.clone())
            .collect();
        for name in names {
            self.manual_refresh_one(&name);
        }
    }

    /// Queue a single profile for an immediate re-fetch (Usage `r`).
    pub(crate) fn manual_refresh_one(&self, name: &str) {
        // Light the spinner immediately so the UI reflects the keypress.
        // Only when idle — don't clobber an in-flight switch/refresh marker.
        if is_idle(&self.activity, name) {
            mark_activity(&self.activity, name, ProfileActivity::Fetching);
        }
        if let Ok(mut q) = self.refetch_queue.lock() {
            q.insert(name.to_string());
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

    /// Disarm the 2-step quit and clear its accompanying footer alert.
    /// Only call this when the armed-quit sequence is interrupted (key switch,
    /// tab change, etc.). For `x`-dismiss of an alert produced by a non-quit
    /// condition, clear `footer_alert` directly instead.
    pub(crate) fn disarm_quit(&mut self) {
        self.armed_quit = false;
        self.footer_alert = None;
    }

    pub(crate) fn prune_toasts(&mut self) {
        while let Some(front) = self.toasts.front() {
            let ttl = if front.kind == ToastKind::Danger {
                TOAST_TTL_DANGER
            } else {
                TOAST_TTL_NORMAL
            };
            if front.born.elapsed() >= ttl {
                self.toasts.pop_front();
            } else {
                break;
            }
        }
    }

    /// Called each frame with the current terminal height. Tracks compact
    /// state; the matching warning banner is recomputed from this flag in
    /// `update_banner` each tick, so it self-clears on resize (no toast).
    pub(crate) fn update_compact(&mut self, terminal_height: u16) {
        self.compact = terminal_height < 14;
    }

    /// Mark a background tab with an activity color. Only sets if `tab` is not
    /// the currently-active tab; visiting the tab (via `switch_tab`) clears it.
    /// Higher-severity kinds override lower ones (Danger > Warning > Success > Info).
    pub(crate) fn set_tab_activity(&mut self, tab: Tab, kind: ToastKind) {
        if tab == self.tab {
            return;
        }
        let idx = tab.index();
        let prev = self.tab_activity[idx];
        let severity = |k: ToastKind| match k {
            ToastKind::Danger => 3,
            ToastKind::Warning => 2,
            ToastKind::Success => 1,
            ToastKind::Info => 0,
        };
        if prev.is_none_or(|p| severity(kind) > severity(p)) {
            self.tab_activity[idx] = Some(kind);
        }
    }

    // ── Main list ────────────────────────────────────────────────────────────

    pub(crate) fn main_items(&self) -> Vec<MainItemKind> {
        (0..self.config().profiles.len())
            .map(MainItemKind::Profile)
            .collect()
    }

    /// Number of profiles.
    pub(crate) fn profile_count(&self) -> usize {
        self.config().profiles.len()
    }

    /// Profile name at `idx`, if any.
    pub(crate) fn profile_name_at(&self, idx: usize) -> Option<String> {
        self.config().profiles.get(idx).map(|p| p.name.to_string())
    }

    /// Clamp `profile_cursor` to `0..profile_count`.
    pub(crate) fn clamp_profile_cursor(&mut self) {
        let max = self.profile_count().saturating_sub(1);
        self.profile_cursor = self.profile_cursor.min(max);
    }

    pub(crate) fn current_main_item(&self) -> Option<MainItemKind> {
        self.main_items().get(self.profile_cursor).copied()
    }
}

// ── Token snapshot ────────────────────────────────────────────────────────────

fn collect_tokens(profiles: &[Profile]) -> Vec<TokenEntry> {
    profiles
        .iter()
        .filter_map(|p| {
            let oauth = p.credentials.as_ref()?.claude_ai_oauth.as_ref()?;
            Some(TokenEntry {
                name: p.name.to_string(),
                access_token: oauth.access_token.clone(),
                refresh_token: oauth.refresh_token.clone(),
            })
        })
        .collect()
}

// ── Startup reconciliation ────────────────────────────────────────────────────

/// Startup credential reconciliation, non-blocking. Compares the live
/// `~/.claude/.credentials.json` to the active profile's stored creds inline.
/// No divergence → snapshot + `ReconcileDone` immediately.
///
/// On divergence we never probe the stored chain via OAuth refresh — a refresh
/// *spends* the single-use refresh token server-side and would rotate the stored
/// identity on every diverged startup. Instead the Divergence modal hands the
/// verdict to the user. None of its actions spend the stored token (Overwrite
/// takes live creds, NewProfile captures them, Discard relinks as-is); a stale
/// access token is refreshed lazily on the next fetch.
pub(super) fn reconcile_startup(app: &mut App) {
    let Some(active) = app.config().state.active_profile.clone() else {
        let _ = app.startup_sender.send(StartupSignal::ReconcileDone);
        return;
    };

    // Read live credentials under the state lock to avoid torn snapshots.
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

    // Diverged: hand the verdict to the user. No network, no FS write here.
    let _ = app
        .startup_sender
        .send(StartupSignal::ReconcileNeedsPrompt {
            active: active.to_string(),
        });
}

// ── Event handling ────────────────────────────────────────────────────────────

pub(crate) fn handle_key(app: &mut App, key: KeyEvent) {
    if key.kind != KeyEventKind::Press {
        return;
    }

    // Ctrl-C always exits.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.quit = true;
        return;
    }

    if !app.modals.is_empty() {
        handle_modal_key(app, key);
        return;
    }

    // Config text field capturing keystrokes owns keyboard (like a modal)
    // so typing into a name can't fire global shortcuts.
    if app.tab == Tab::Setup
        && app.config_focus == ConfigFocus::Actions
        && app
            .config_draft
            .as_ref()
            .is_some_and(|d| d.active.is_some())
    {
        handle_config_edit_key(app, key);
        return;
    }

    // Same for the threshold editor: owns keyboard so digits can't trip globals.
    if app.tab == Tab::Fallback
        && app.fallback_focus == FallbackFocus::Detail
        && app.fallback_threshold_draft.is_some()
    {
        handle_fallback_threshold_edit_key(app, key);
        return;
    }

    // Global keys (no modal owns input).
    match key.code {
        KeyCode::Right => {
            app.disarm_quit();
            switch_tab(app, app.tab.next());
            return;
        }
        KeyCode::Left => {
            app.disarm_quit();
            switch_tab(app, app.tab.prev());
            return;
        }
        KeyCode::Char('?') => {
            app.disarm_quit();
            app.modals.push(Modal::Help);
            return;
        }
        KeyCode::Char('a') => {
            app.disarm_quit();
            let state = build_action_menu(app);
            if !state.items.is_empty() {
                app.modals.push(Modal::ActionMenu(state));
            }
            return;
        }
        KeyCode::Char('r') => {
            app.disarm_quit();
            // Status `r` refreshes the whole feed.
            if app.tab == Tab::Status {
                trigger_status_refresh(app);
                return;
            }
            // Usage `r` refreshes only the current account.
            if app.tab == Tab::Usage {
                let selected = {
                    let cfg = app.config();
                    cfg.profiles
                        .get(app.profile_cursor)
                        .map(|p| (p.name.clone(), p.is_oauth(), p.is_third_party()))
                };
                match selected {
                    Some((name, true, _)) | Some((name, _, true)) => {
                        app.manual_refresh_one(&name);
                        app.toast(ToastKind::Info, format!("refreshing '{name}'…"));
                    }
                    Some((name, false, false)) => {
                        app.toast(ToastKind::Info, format!("'{name}' has no usage to refresh"));
                    }
                    None => {}
                }
            } else {
                app.manual_refresh();
                app.toast(ToastKind::Info, "refreshing usage…");
            }
            return;
        }
        KeyCode::Char('t') => {
            app.disarm_quit();
            app.modals.push(Modal::Confirm(ConfirmState {
                message: ROTATE_ALL_MSG.to_string(),
                detail: Some(ROTATE_ALL_DETAIL.to_string()),
                choice: false,
                on_confirm: ConfirmAction::RotateAll,
            }));
            return;
        }
        KeyCode::Char('n') => {
            app.disarm_quit();
            start_new_account(app);
            return;
        }
        // Esc backs out of sub-focus; no-op at the top level.
        KeyCode::Esc => {
            app.disarm_quit();
            if app.tab == Tab::Setup && app.config_focus == ConfigFocus::Actions {
                app.config_focus = ConfigFocus::Profiles;
                app.config_draft = None;
            } else if app.tab == Tab::Fallback && app.fallback_focus == FallbackFocus::Detail {
                leave_fallback_detail(app);
            } else if app.tab == Tab::Status && app.status.focus == StatusFocus::Detail {
                app.status.focus = StatusFocus::List;
            }
            // At top level, Esc is a no-op — ctrl+c or `q q` to quit.
            return;
        }
        // First `q` at the top level arms the 2-step quit; second confirms.
        // When there is a sub-focus to back out of, `q` ascends instead.
        KeyCode::Char('q') => {
            let has_sub_focus = (app.tab == Tab::Setup && app.config_focus == ConfigFocus::Actions)
                || (app.tab == Tab::Fallback && app.fallback_focus == FallbackFocus::Detail)
                || (app.tab == Tab::Status && app.status.focus == StatusFocus::Detail);
            if has_sub_focus {
                app.disarm_quit();
                if app.tab == Tab::Setup {
                    app.config_focus = ConfigFocus::Profiles;
                    app.config_draft = None;
                } else if app.tab == Tab::Fallback {
                    leave_fallback_detail(app);
                } else {
                    app.status.focus = StatusFocus::List;
                }
            } else if app.armed_quit {
                app.quit = true;
            } else {
                app.armed_quit = true;
                app.footer_alert = Some(FooterAlert::Warn("press q again to quit".to_string()));
            }
            return;
        }
        KeyCode::Char('x') => {
            // Dismissal precedence: toasts first, then footer alert.
            // `x` dismisses the alert directly; quit disarming is a side effect
            // only when armed_quit was the producer — not a general alert clear.
            if !app.toasts.is_empty() {
                app.toasts.pop_front();
            } else if app.footer_alert.is_some() {
                app.footer_alert = None;
                if app.armed_quit {
                    app.armed_quit = false;
                }
            }
            return;
        }
        _ => {
            // Any unhandled key disarms the 2-step quit.
            app.disarm_quit();
        }
    }

    match app.tab {
        Tab::Overview => handle_overview_key(app, key),
        Tab::Usage => handle_usage_key(app, key),
        Tab::Setup => handle_config_key(app, key),
        Tab::Fallback => handle_fallback_key(app, key),
        Tab::Config => handle_global_config_key(app, key),
        Tab::Status => handle_status_key(app, key),
    }
}

/// Switch the active tab and reset that tab's cursor to a sensible default.
fn switch_tab(app: &mut App, tab: Tab) {
    app.tab = tab;
    app.tab_activity[tab.index()] = None;
    app.config_draft = None;
    // Clamp cursor: a Config `+ new` selection must land on a real account.
    app.clamp_profile_cursor();
    match tab {
        Tab::Overview | Tab::Usage => {}
        Tab::Setup => {
            app.config_focus = ConfigFocus::Profiles;
            app.config_action_cursor = 0;
        }
        Tab::Fallback => {
            app.chain_cursor = chain_cursor_for_profile(app);
            sync_profile_from_chain(app);
            app.fallback_focus = FallbackFocus::Chain;
            app.fallback_detail_cursor = 0;
            app.fallback_armed_remove = false;
            app.fallback_threshold_draft = None;
        }
        Tab::Config => {
            app.global_config_cursor = 0;
        }
        Tab::Status => {
            // Keep the incident cursor; reset focus to the list per the contract.
            app.status.focus = StatusFocus::List;
        }
    }
}

/// Move `profile_cursor` by `delta`, wrapping in `0..len`.
fn step_profile_cursor(app: &mut App, delta: i32, len: usize) {
    if len == 0 {
        return;
    }
    app.profile_cursor = (app.profile_cursor as i32 + delta).rem_euclid(len as i32) as usize;
}

fn handle_overview_key(app: &mut App, key: KeyEvent) {
    let count = app.profile_count();
    match key.code {
        KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => reorder_main_cursor(app, -1),
        KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => reorder_main_cursor(app, 1),
        KeyCode::Up => step_profile_cursor(app, -1, count),
        KeyCode::Down => step_profile_cursor(app, 1, count),
        KeyCode::Enter => activate_main_item(app),
        _ => {}
    }
}

/// Usage tab: up/down picks the account. Read-only pane.
fn handle_usage_key(app: &mut App, key: KeyEvent) {
    let count = app.profile_count();
    match key.code {
        KeyCode::Up => step_profile_cursor(app, -1, count),
        KeyCode::Down => step_profile_cursor(app, 1, count),
        _ => {}
    }
}

/// Status tab keymap. List focus: ↑↓ moves the incident cursor (wrapping), ⏎
/// descends into the timeline (only when an incident exists). Detail focus: ↑↓
/// scrolls the timeline (clamped to content by the render pass).
fn handle_status_key(app: &mut App, key: KeyEvent) {
    match app.status.focus {
        StatusFocus::List => {
            let len = app.status.incidents.len();
            match key.code {
                KeyCode::Up if len > 0 => {
                    app.status.cursor = (app.status.cursor + len - 1).rem_euclid(len.max(1));
                    app.status.detail_scroll = 0;
                }
                KeyCode::Down if len > 0 => {
                    app.status.cursor = (app.status.cursor + 1).rem_euclid(len.max(1));
                    app.status.detail_scroll = 0;
                }
                KeyCode::Enter if len > 0 => {
                    app.status.focus = StatusFocus::Detail;
                    // Open at the top of the timeline.
                    app.status.detail_scroll = 0;
                }
                _ => {}
            }
        }
        StatusFocus::Detail => match key.code {
            KeyCode::Up => {
                app.status.detail_scroll = app.status.detail_scroll.saturating_sub(1);
            }
            KeyCode::Down => {
                // Clamp against the last render's content height so a held ↓ can't
                // run the offset past the end (which would make ↑ look dead).
                let max = app.status.detail_max_scroll.get();
                app.status.detail_scroll = app.status.detail_scroll.saturating_add(1).min(max);
            }
            _ => {}
        },
    }
}

/// Signal the status thread to refetch and light the title spinner.
fn trigger_status_refresh(app: &mut App) {
    let _ = app.status_refresh.send(());
    app.status.fetching = true;
    app.toast(ToastKind::Info, "refreshing status…");
}

/// Open the selected incident's page in the default browser (detached).
fn open_incident_link(app: &mut App) {
    let Some(link) = app.status.selected().map(|i| i.link.clone()) else {
        return;
    };
    if link.is_empty() {
        app.toast(ToastKind::Info, "no link for this incident");
        return;
    }
    let spawned = std::process::Command::new("xdg-open")
        .arg(&link)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match spawned {
        Ok(_) => app.toast(ToastKind::Info, "opening in browser"),
        Err(_) => app.toast(ToastKind::Danger, "failed to open browser"),
    }
}

/// Request switch to profile at `idx`; no-ops if already active.
fn request_switch_to(app: &mut App, idx: usize) {
    let cfg = app.config();
    let Some(name) = cfg.profiles.get(idx).map(|p| p.name.to_string()) else {
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
        // No-op when already active.
        MainItemKind::Profile(idx) => request_switch_to(app, idx),
    }
}

fn reorder_main_cursor(app: &mut App, delta: i32) {
    // Only reorder real profile rows, not the action rows.
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
    // Cursor follows the moved row.
    if delta < 0 && app.profile_cursor > 0 {
        app.profile_cursor -= 1;
    } else if delta > 0 {
        app.profile_cursor += 1;
    }
}

/// Spawn a worker under `catch_unwind`. On panic, clears the activity slot and
/// emits a failure `OpResult` so a panic before `sender.send` never strands the
/// slot (which would wedge `any_busy`). `AssertUnwindSafe` is intentional —
/// captured Arcs have their own locks; a panic can't violate other threads.
fn spawn_profile_worker<F>(
    name: String,
    kind: ActivityKind,
    panic_msg: &'static str,
    activity: ActivityStore,
    sender: OpResultSender,
    work: F,
) where
    F: FnOnce() + Send + 'static,
{
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
        if result.is_err() {
            clear_activity(&activity, &name);
            let _ = sender.send(OpResult {
                name,
                kind,
                outcome: Err(anyhow::anyhow!(panic_msg)),
            });
        }
    });
}

/// Switch the active profile to `name`. Pure filesystem relink, no token
/// rotation (401-recovery handles that lazily). Runs on the UI thread.
fn perform_switch(app: &mut App, name: &str) {
    finalize_switch(app, name);
}

/// True when the live credentials diverge from the stored chain and it's not a
/// first-login adoption (must be reconciled before clearing/relinking).
fn active_diverged_unsaved(active: &str) -> bool {
    matches!(
        classify_credentials_link(active).ok(),
        Some(LinkState::Diverged)
    ) && !is_first_login(active).unwrap_or(false)
}

/// Toast and raise the Divergence prompt for `active` (`verb` = blocked action).
fn prompt_divergence(app: &mut App, active: String, verb: &str) {
    app.toast(
        ToastKind::Warning,
        format!("'{active}' has unsaved Claude Code credentials — resolve before {verb}"),
    );
    app.modals
        .push(Modal::Divergence(DivergenceForm { active, cursor: 0 }));
}

/// Synchronous UI-thread switch. No HTTP; clears the Switching marker, runs
/// `switch_profile`, refreshes token snapshot on success.
fn finalize_switch(app: &mut App, name: &str) {
    // Guard a diverged outgoing active: `switch_profile` would no-op the
    // snapshot and then `link_profile_credentials` would bail on the regular
    // file, stranding the fresh `/login` chain. Raise the Divergence modal so
    // the user cleans up first; first-login adoption stays a clean switch.
    let outgoing = app.config().state.active_profile.clone();
    if let Some(active) = outgoing
        && active != name
        && active_diverged_unsaved(&active)
    {
        clear_activity(&app.activity, name);
        prompt_divergence(app, active.to_string(), "switching");
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

/// Turn off all accounts on the UI thread. Mirrors `finalize_switch`'s
/// divergence guard: an unsaved `/login` must be resolved before clearing live
/// credentials. No HTTP.
fn perform_switch_off(app: &mut App) {
    let Some(active) = app.config().state.active_profile.clone() else {
        return;
    };
    if active_diverged_unsaved(&active) {
        prompt_divergence(app, active.to_string(), "switching off");
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

/// Detail rows for a chain member: threshold stepper, remove.
pub(crate) const FALLBACK_ROWS: [FallbackRow; 2] = [FallbackRow::Threshold, FallbackRow::Remove];

/// Rows on the program-wide Config tab, in display order.
pub(crate) const GLOBAL_CONFIG_ROWS: [GlobalConfigRow; 2] =
    [GlobalConfigRow::Theme, GlobalConfigRow::WrapOff];

/// Config tab keymap: ↑↓ walks rows, ⏎/space cycles the theme or flips wrap-off.
fn handle_global_config_key(app: &mut App, key: KeyEvent) {
    let last = GLOBAL_CONFIG_ROWS.len() - 1;
    app.global_config_cursor = app.global_config_cursor.min(last);
    match key.code {
        KeyCode::Up => {
            app.global_config_cursor = if app.global_config_cursor == 0 {
                last
            } else {
                app.global_config_cursor - 1
            };
        }
        KeyCode::Down => {
            app.global_config_cursor = if app.global_config_cursor >= last {
                0
            } else {
                app.global_config_cursor + 1
            };
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            run_global_config_row(app, GLOBAL_CONFIG_ROWS[app.global_config_cursor]);
        }
        _ => {}
    }
}

/// Apply ⏎/space on a Config-tab row: cycle the theme tier or flip wrap-off.
fn run_global_config_row(app: &mut App, row: GlobalConfigRow) {
    match row {
        GlobalConfigRow::Theme => cycle_theme(app),
        GlobalConfigRow::WrapOff => toggle_wrap_off(app),
    }
}

/// Cycle the active theme tier, persist it to `[theme]`, and live-swap the
/// palette so the next frame renders in the new tier without a restart.
fn cycle_theme(app: &mut App) {
    let next = match theme::tier() {
        theme::Tier::Full => theme::Tier::Compatible,
        theme::Tier::Compatible => theme::Tier::Full,
    };
    let name = match next {
        theme::Tier::Full => ThemeName::Full,
        theme::Tier::Compatible => ThemeName::Compatible,
    };
    {
        let mut cfg = app.config();
        cfg.state.theme = Some(name);
        let _ = save_app_state(&cfg.state);
    }
    app.last_state_mtime = app_state_mtime();
    theme::set_tier(next);
}

/// Fallback footer hint derived from current focus + selection + edit state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackHint {
    Empty,
    ChainMember,
    ChainAdd,
    DetailThreshold,
    DetailThresholdEdit,
    DetailRemove,
    DetailRemoveArmed,
    DetailAdd,
}

/// Resolve the Fallback tab's footer hint.
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
                FallbackRow::Remove if app.fallback_armed_remove => FallbackHint::DetailRemoveArmed,
                FallbackRow::Remove => FallbackHint::DetailRemove,
            }
        }
    }
}

/// Fallback tab keymap; delegates to chain or detail handler.
fn handle_fallback_key(app: &mut App, key: KeyEvent) {
    match app.fallback_focus {
        FallbackFocus::Chain => handle_fallback_chain_key(app, key),
        FallbackFocus::Detail => handle_fallback_detail_key(app, key),
    }
}

/// Returns the chain row that should be highlighted when entering the fallback tab.
/// If the currently-selected profile (`profile_cursor`) is a member of the chain,
/// returns its position there; otherwise 0.
fn chain_cursor_for_profile(app: &App) -> usize {
    let cfg = app.config();
    let selected_name = cfg
        .profiles
        .get(app.profile_cursor)
        .map(|p| p.name.as_str());
    if let Some(name) = selected_name
        && let Some(pos) = cfg.state.fallback_chain.iter().position(|c| c == name)
    {
        return pos;
    }
    0
}

/// If the current `chain_cursor` points at a `Member`, sync `profile_cursor` to that
/// member's index in the profile list. Leaves `profile_cursor` unchanged on `Add` or
/// when the name is not found (e.g. empty chain).
fn sync_profile_from_chain(app: &mut App) {
    let chain_pos = app.chain_cursor;
    let profile_idx = {
        let cfg = app.config();
        match cfg.state.fallback_chain.get(chain_pos) {
            Some(name) => cfg.profiles.iter().position(|p| p.name == *name),
            None => None,
        }
    };
    if let Some(idx) = profile_idx {
        app.profile_cursor = idx;
    }
}

fn handle_fallback_chain_key(app: &mut App, key: KeyEvent) {
    let last = chain_items(app).len().saturating_sub(1);
    match key.code {
        KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => reorder_chain_member(app, -1),
        KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
            reorder_chain_member(app, 1)
        }
        KeyCode::Up => {
            app.chain_cursor = if app.chain_cursor == 0 {
                last
            } else {
                app.chain_cursor - 1
            };
            sync_profile_from_chain(app);
        }
        KeyCode::Down => {
            app.chain_cursor = if app.chain_cursor >= last {
                0
            } else {
                app.chain_cursor + 1
            };
            sync_profile_from_chain(app);
        }
        KeyCode::Enter => enter_fallback_detail(app),
        _ => {}
    }
}

/// Toggle wrap-off: on = switch off all when chain is spent; off = stay on last.
fn toggle_wrap_off(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.wrap_off = !cfg.state.wrap_off;
        let _ = save_app_state(&cfg.state);
    }
    app.last_state_mtime = app_state_mtime();
}

/// Right-pane keymap for a member: ↑↓ walks rows, `+`/`-` steps the threshold,
/// ⏎/space on remove arms then confirms. Delegates to add picker on `+ add`.
fn handle_fallback_detail_key(app: &mut App, key: KeyEvent) {
    if selected_chain_member(app).is_none() {
        handle_fallback_add_key(app, key);
        return;
    }
    let last = FALLBACK_ROWS.len() - 1;
    app.fallback_detail_cursor = app.fallback_detail_cursor.min(last);
    let on_threshold = FALLBACK_ROWS[app.fallback_detail_cursor] == FallbackRow::Threshold;
    match key.code {
        KeyCode::Up => {
            app.fallback_armed_remove = false;
            app.fallback_detail_cursor = if app.fallback_detail_cursor == 0 {
                last
            } else {
                app.fallback_detail_cursor - 1
            };
        }
        KeyCode::Down => {
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
        KeyCode::Up => {
            app.fallback_detail_cursor = if app.fallback_detail_cursor == 0 {
                last
            } else {
                app.fallback_detail_cursor - 1
            };
        }
        KeyCode::Down => {
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
            // When the picker empties, `+ add` disappears — land on the new member.
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

/// Chain index under the cursor, or `None` on `+ add`.
fn selected_chain_member(app: &App) -> Option<usize> {
    match chain_items(app).get(app.chain_cursor).copied() {
        Some(ChainItemKind::Member(i)) => Some(i),
        _ => None,
    }
}

/// Profiles not yet in the chain (add-picker candidates).
pub(crate) fn chain_candidates(app: &App) -> Vec<String> {
    let cfg = app.config();
    cfg.profiles
        .iter()
        .filter(|p| !cfg.state.fallback_chain.iter().any(|c| c == &p.name))
        .map(|p| p.name.to_string())
        .collect()
}

/// Enter right pane for the selected chain item. No-op on `+ add` when empty.
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

/// Return focus to the chain list, clearing any armed remove or live edit.
fn leave_fallback_detail(app: &mut App) {
    app.fallback_focus = FallbackFocus::Chain;
    app.fallback_armed_remove = false;
    app.fallback_detail_cursor = 0;
    app.fallback_threshold_draft = None;
}

/// ⇧↑↓: move the selected member up/down, cursor follows. No-op on `+ add`
/// or at boundary. Chain index == cursor for members (they precede `+ add`).
fn reorder_chain_member(app: &mut App, delta: i32) {
    let Some(pos) = selected_chain_member(app) else {
        return;
    };
    let target = pos as i32 + delta;
    {
        let mut cfg = app.config();
        if target < 0 || target as usize >= cfg.state.fallback_chain.len() {
            return;
        }
        cfg.state.fallback_chain.swap(pos, target as usize);
        let _ = save_app_state(&cfg.state);
    }
    app.chain_cursor = target as usize;
}

/// ⏎/space on a member detail row: threshold opens inline editor; remove arms
/// then deletes on second press.
fn run_fallback_row(app: &mut App, row: FallbackRow) {
    match row {
        FallbackRow::Threshold => {
            if let Some(current) = selected_threshold(app) {
                app.fallback_threshold_draft = Some(InputState::new(&format!("{current:.0}")));
            }
        }
        FallbackRow::Remove => {
            if app.fallback_armed_remove {
                remove_chain_member(app);
            } else {
                app.fallback_armed_remove = true;
            }
        }
    }
}

/// Keystrokes while the threshold field is open: ⏎ saves, ⎋ discards.
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

/// Parse and persist the typed threshold (0..=100). Invalid input keeps the
/// draft open so the inline Invalid-input treatment (DANGER value + `└ max is …`
/// tooltip, rendered by the detail card) stays on screen until corrected — no toast.
fn commit_threshold_edit(app: &mut App) {
    let Some(raw) = app.fallback_threshold_draft.as_ref().map(|i| i.trimmed()) else {
        return;
    };
    let Some(value) = parse_threshold(raw) else {
        return;
    };
    write_threshold(app, value);
    app.fallback_threshold_draft = None;
}

/// A typed threshold is valid only as a number in `0..=100`. Shared by the
/// commit path and the detail card's inline Invalid-input check.
pub(crate) fn parse_threshold(raw: &str) -> Option<f64> {
    raw.parse::<f64>()
        .ok()
        .filter(|v| (0.0..=100.0).contains(v))
}

/// Effective threshold for the selected member, or `None` on `+ add`.
fn selected_threshold(app: &App) -> Option<f64> {
    let pos = selected_chain_member(app)?;
    let cfg = app.config();
    let name = cfg.state.fallback_chain.get(pos)?;
    cfg.find(name).map(threshold_for)
}

/// Write threshold for the selected member and persist.
fn write_threshold(app: &mut App, value: f64) {
    let Some(pos) = selected_chain_member(app) else {
        return;
    };
    let save_err = {
        let mut cfg = app.config();
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

/// Step the threshold by `delta`, clamped to 0..=100, and persist.
fn adjust_threshold(app: &mut App, delta: f64) {
    if let Some(current) = selected_threshold(app) {
        write_threshold(app, (current + delta).clamp(0.0, 100.0));
    }
}

/// Add a profile to the chain (seeding default threshold if unset) and persist.
fn add_chain_candidate(app: &mut App, name: &str) {
    let mut cfg = app.config();
    if let Some(profile) = cfg.find_mut(name)
        && profile.fallback_threshold.is_none()
    {
        profile.fallback_threshold = Some(DEFAULT_THRESHOLD);
        let _ = save_profile(profile);
    }
    cfg.state.fallback_chain.push(name.into());
    let _ = save_app_state(&cfg.state);
}

/// Remove the selected member, persist, and return focus to the list.
fn remove_chain_member(app: &mut App) {
    let Some(pos) = selected_chain_member(app) else {
        return;
    };
    let name = {
        let mut cfg = app.config();
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
        Modal::ActionMenu(_) => handle_action_menu_key(app, key),
    }
}

/// Build the action menu for the current screen/focus context.
fn build_action_menu(app: &App) -> ActionMenuState {
    use ActionMenuAction::*;
    let mut actions: Vec<ActionMenuAction> = Vec::new();

    match app.tab {
        Tab::Overview => {
            // Switch is only useful when the cursor is on a non-active profile.
            let can_switch = app
                .current_main_item()
                .map(|item| match item {
                    MainItemKind::Profile(idx) => {
                        let cfg = app.config();
                        cfg.profiles
                            .get(idx)
                            .map(|p| !cfg.is_active(&p.name))
                            .unwrap_or(false)
                    }
                })
                .unwrap_or(false);
            if can_switch {
                actions.push(SwitchToSelected);
            }
            actions.push(NewAccount);
            actions.push(RefreshUsage);
            actions.push(RotateTokens);
        }
        Tab::Usage => {
            actions.push(RefreshUsage);
        }
        Tab::Setup => match app.config_focus {
            ConfigFocus::Profiles => {
                if app.profile_cursor < app.profile_count() {
                    actions.push(ConfigureSelected);
                }
                actions.push(NewAccount);
            }
            ConfigFocus::Actions => {
                // In the actions detail pane, actions depend on the focused row.
                let rows = config_rows(app);
                if let Some(&row) = rows.get(app.config_action_cursor) {
                    match row {
                        ConfigRow::AutoStart => actions.push(ActionMenuAction::ToggleAutoStart),
                        ConfigRow::Delete => actions.push(ActionMenuAction::DeleteProfile),
                        ConfigRow::Create => actions.push(ActionMenuAction::CreateProfile),
                        _ => actions.push(ActionMenuAction::EditField),
                    }
                }
            }
        },
        Tab::Fallback => match app.fallback_focus {
            FallbackFocus::Chain => {
                let items = chain_items(app);
                if let Some(item) = items.get(app.chain_cursor) {
                    match item {
                        ChainItemKind::Member(_) => {
                            actions.push(OpenChainMember);
                            actions.push(ReorderUp);
                            actions.push(ReorderDown);
                        }
                        ChainItemKind::Add => {}
                    }
                }
            }
            FallbackFocus::Detail => {
                if let Some(&row) = FALLBACK_ROWS.get(app.fallback_detail_cursor) {
                    match row {
                        FallbackRow::Threshold => actions.push(EditThreshold),
                        FallbackRow::Remove => actions.push(RemoveMember),
                    }
                }
            }
        },
        Tab::Config => {
            if let Some(&row) = GLOBAL_CONFIG_ROWS.get(app.global_config_cursor) {
                match row {
                    GlobalConfigRow::Theme => actions.push(CycleTheme),
                    GlobalConfigRow::WrapOff => actions.push(ToggleWrapOff),
                }
            }
        }
        Tab::Status => {
            actions.push(RefreshStatus);
            if app.status.selected().is_some() {
                actions.push(OpenIncidentLink);
            }
        }
    }

    ActionMenuState::new(actions)
}

fn handle_action_menu_key(app: &mut App, key: KeyEvent) {
    // Try a direct hotkey press first.
    if let KeyCode::Char(ch) = key.code {
        let ch_lower = ch.to_lowercase().next().unwrap_or(ch);
        let Some(Modal::ActionMenu(state)) = app.modals.last() else {
            return;
        };
        let action = state
            .items
            .iter()
            .find(|item| item.hotkey == Some(ch_lower))
            .map(|item| item.action.clone());
        if let Some(action) = action {
            app.modals.pop();
            dispatch_action_menu_action(app, action);
            return;
        }
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.modals.pop();
        }
        KeyCode::Up => {
            let Some(Modal::ActionMenu(state)) = app.modals.last_mut() else {
                return;
            };
            let len = state.items.len();
            if len > 0 {
                state.cursor = (state.cursor + len - 1) % len;
            }
        }
        KeyCode::Down => {
            let Some(Modal::ActionMenu(state)) = app.modals.last_mut() else {
                return;
            };
            let len = state.items.len();
            if len > 0 {
                state.cursor = (state.cursor + 1) % len;
            }
        }
        KeyCode::Enter => {
            let Some(Modal::ActionMenu(state)) = app.modals.last() else {
                return;
            };
            let action = state
                .items
                .get(state.cursor)
                .map(|item| item.action.clone());
            app.modals.pop();
            if let Some(action) = action {
                dispatch_action_menu_action(app, action);
            }
        }
        _ => {}
    }
}

/// Fire the handler that the direct hotkey would have called.
/// The account under the cursor as `(name, is_oauth, is_third_party)`. `profile_cursor` is shared
/// across Overview, Usage, and Setup, so this resolves the focused account on any
/// of them. `None` when the cursor sits past the profile list (e.g. `+ new`).
fn focused_account(app: &App) -> Option<(String, bool, bool)> {
    let cfg = app.config();
    cfg.profiles
        .get(app.profile_cursor)
        .map(|p| (p.name.to_string(), p.is_oauth(), p.is_third_party()))
}

fn dispatch_action_menu_action(app: &mut App, action: ActionMenuAction) {
    match action {
        ActionMenuAction::NewAccount => start_new_account(app),
        ActionMenuAction::RefreshUsage => {
            // Action-menu refresh is always scoped to the focused account.
            match focused_account(app) {
                Some((name, _, true)) | Some((name, true, _)) => {
                    app.manual_refresh_one(&name);
                    app.toast(ToastKind::Info, format!("refreshing '{name}'…"));
                }
                Some((name, false, false)) => {
                    app.toast(ToastKind::Info, format!("'{name}' has no usage to refresh"));
                }
                None => {}
            }
        }
        ActionMenuAction::RotateTokens => {
            // Rotate only the focused account, not the whole chain.
            match focused_account(app) {
                Some((name, true, _)) => {
                    app.modals.push(Modal::Confirm(ConfirmState {
                        message: format!("rotate tokens for '{name}'?"),
                        detail: Some(ROTATE_ONE_DETAIL.to_string()),
                        choice: false,
                        on_confirm: ConfirmAction::RotateOne(name),
                    }));
                }
                Some((name, _, _)) => {
                    app.toast(ToastKind::Info, format!("'{name}' has no tokens to rotate"));
                }
                None => {}
            }
        }
        ActionMenuAction::SwitchToSelected => activate_main_item(app),
        ActionMenuAction::ConfigureSelected => enter_config_detail(app),
        ActionMenuAction::OpenChainMember => enter_fallback_detail(app),
        ActionMenuAction::ReorderUp => reorder_chain_member(app, -1),
        ActionMenuAction::ReorderDown => reorder_chain_member(app, 1),
        ActionMenuAction::EditThreshold => {
            run_fallback_row(app, FallbackRow::Threshold);
        }
        ActionMenuAction::ToggleWrapOff => {
            toggle_wrap_off(app);
        }
        ActionMenuAction::RemoveMember => {
            run_fallback_row(app, FallbackRow::Remove);
        }
        ActionMenuAction::ToggleAutoStart => {
            let rows = config_rows(app);
            if let Some(&row) = rows.get(app.config_action_cursor) {
                run_config_row(app, row);
            }
        }
        ActionMenuAction::DeleteProfile => {
            let rows = config_rows(app);
            if let Some(&row) = rows.get(app.config_action_cursor) {
                run_config_row(app, row);
            }
        }
        ActionMenuAction::CreateProfile => {
            let rows = config_rows(app);
            if let Some(&row) = rows.get(app.config_action_cursor) {
                run_config_row(app, row);
            }
        }
        ActionMenuAction::EditField => {
            let rows = config_rows(app);
            if let Some(&row) = rows.get(app.config_action_cursor) {
                run_config_row(app, row);
            }
        }
        ActionMenuAction::CycleTheme => cycle_theme(app),
        ActionMenuAction::RefreshStatus => trigger_status_refresh(app),
        ActionMenuAction::OpenIncidentLink => open_incident_link(app),
    }
}

/// Setup tab keymap. Left: ↑↓ + ⏎ enters detail. Right: ↑↓ walks rows, ⏎
/// edits/toggles/arms/creates. Esc (global) returns to list.
fn handle_config_key(app: &mut App, key: KeyEvent) {
    // Selector includes the trailing `+ new` row.
    let sel_len = app.profile_count() + 1;
    app.profile_cursor = app.profile_cursor.min(sel_len - 1);

    match app.config_focus {
        ConfigFocus::Profiles => match key.code {
            KeyCode::Up => step_profile_cursor(app, -1, sel_len),
            KeyCode::Down => step_profile_cursor(app, 1, sel_len),
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
                KeyCode::Up => {
                    disarm_delete(app);
                    app.config_action_cursor = if app.config_action_cursor == 0 {
                        last
                    } else {
                        app.config_action_cursor - 1
                    };
                }
                KeyCode::Down => {
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

/// Detail rows for the current selection. `+ new` → create form; account →
/// settings (auto-start only for OAuth).
pub(crate) fn config_rows(app: &App) -> Vec<ConfigRow> {
    let cfg = app.config();
    if app.profile_cursor >= cfg.profiles.len() {
        return vec![
            ConfigRow::Name,
            ConfigRow::BaseUrl,
            ConfigRow::ApiKey,
            ConfigRow::Create,
        ];
    }
    let is_oauth = cfg
        .profiles
        .get(app.profile_cursor)
        .map(|p| p.is_oauth())
        .unwrap_or(true);
    let mut rows = vec![ConfigRow::Name, ConfigRow::BaseUrl, ConfigRow::ApiKey];
    if is_oauth {
        rows.push(ConfigRow::AutoStart);
    }
    rows.push(ConfigRow::Delete);
    rows
}

/// Enter the detail pane, seeding a draft for the current selection.
fn enter_config_detail(app: &mut App) {
    app.config_action_cursor = 0;
    if app.profile_cursor >= app.profile_count() {
        app.config_draft = Some(build_draft_new());
    } else if let Some(name) = app.profile_name_at(app.profile_cursor) {
        app.config_draft = Some(build_draft_existing(app, &name));
    } else {
        return;
    }
    app.config_focus = ConfigFocus::Actions;
}

/// Jump to the `+ new` create form (global `n`).
fn start_new_account(app: &mut App) {
    switch_tab(app, Tab::Setup);
    app.profile_cursor = app.profile_count();
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

/// Disarm delete when the cursor moves off the row.
fn disarm_delete(app: &mut App) {
    if let Some(d) = app.config_draft.as_mut() {
        d.armed_delete = false;
    }
}

/// ⏎/space on a detail row: text → capture, toggle → flip, delete → arm/confirm, create → commit.
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

/// Arm the delete row (first ⏎).
fn disarm_delete_inverse(app: &mut App) {
    if let Some(d) = app.config_draft.as_mut() {
        d.armed_delete = true;
    }
}

/// Keystrokes while a text row is active: ⏎ commits, ⎋ reverts.
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

/// ⎋ inside a field: existing accounts revert from the live profile;
/// new drafts keep the typed value. Either way, editing ends.
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

/// ⏎ inside a field: new draft buffers; existing accounts persist per field.
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
        // Keep edit mode open so the user can fix the name.
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
            // Reseed from the saved profile (API key may have been dropped).
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
            app.profile_cursor = new_idx;
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
            app.clamp_profile_cursor();
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
            // Clear the 4.5h cooldown on explicit ON so a prior failed kick
            // doesn't block the scan for hours. The scan arms on the next tick
            // if there's no live window; an existing window is left alone.
            if now_on {
                let mut cfg = app.config();
                cfg.state.last_auto_start_at.remove(name);
                let _ = save_app_state(&cfg.state);
            }
        }
        Outcome::SaveFailed(e) => {
            app.toast(ToastKind::Danger, format!("save failed: {e}"));
        }
    }
}

/// Debounce before kicking a live-session candidate. Exceeds the 60s refresh
/// interval so a CC-opened 5h window lands in the store first.
const AUTO_START_LIVE_SESSION_DEBOUNCE_MS: u64 = 90_000;

/// Filter windowless candidates to those ready to kick. Non-live pass through
/// immediately; live candidates are held for `AUTO_START_LIVE_SESSION_DEBOUNCE_MS`.
/// Timestamps pruned to the live set each call so debounce restarts cleanly.
fn ready_auto_start(app: &mut App, candidates: Vec<String>) -> Vec<String> {
    let live: HashSet<String> = candidates
        .iter()
        .filter(|name| has_live_session(name))
        .cloned()
        .collect();
    debounce_live_candidates(
        &mut app.auto_start_windowless_since,
        candidates,
        &live,
        now_ms(),
    )
}

/// Testable core of [`ready_auto_start`]. `since` tracks first-seen-windowless
/// timestamps and is pruned to `live` so stale timers can't linger.
fn debounce_live_candidates(
    since: &mut HashMap<String, u64>,
    candidates: Vec<String>,
    live: &HashSet<String>,
    now: u64,
) -> Vec<String> {
    since.retain(|name, _| live.contains(name));
    candidates
        .into_iter()
        .filter(|name| {
            if !live.contains(name) {
                return true;
            }
            let first = *since.entry(name.clone()).or_insert(now);
            now.saturating_sub(first) >= AUTO_START_LIVE_SESSION_DEBOUNCE_MS
        })
        .collect()
}

fn handle_confirm_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::Confirm(state)) = app.modals.last_mut() else {
        return;
    };
    match key.code {
        KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
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
            // Refuse if anything is in-flight. Bootstrap is a whole-worker
            // signal separate from per-profile activity; the flag covers the
            // gap between the last Refreshing slot clearing and the next fetch.
            if app.bootstrap_active.load(Ordering::SeqCst) || any_busy(&app.activity) {
                app.toast(
                    ToastKind::Warning,
                    "rotate-all skipped — another op is still in flight",
                );
                return;
            }
            // Spawn the rotate-all worker (HTTP off the UI thread).
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
        ConfirmAction::RotateOne(name) => {
            // The per-profile RotationGuard inside rotate_one serialises against a
            // live session's own refresh, so unlike RotateAll this doesn't need the
            // global any_busy gate — a busy guard surfaces as a Danger toast.
            let config = Arc::clone(&app.config);
            let refetch = Arc::clone(&app.refetch_queue);
            let activity = Arc::clone(&app.activity);
            let sender = app.op_sender.clone();
            let target = name.clone();
            std::thread::spawn(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    oauth::rotate_one(&config, &target, &refetch, &activity, &sender, true);
                }));
            });
            app.toast(ToastKind::Info, format!("rotating '{name}'…"));
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
        KeyCode::Up => {
            state.cursor = if state.cursor == 0 {
                last
            } else {
                state.cursor - 1
            };
        }
        KeyCode::Down => {
            state.cursor = if state.cursor >= last {
                0
            } else {
                state.cursor + 1
            };
        }
        KeyCode::Esc => {
            // Esc dismisses; the 1Hz poll re-pushes if divergence persists.
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
            // Defer detach+deactivate to the capture's success arm so cancel
            // (Esc on the name modal) leaves the prior profile intact.
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
            let Some(Modal::CaptureName(form)) = app.modals.pop() else {
                return;
            };
            let snapshot = *form.snapshot;
            // Divergence capture: detach + deactivate only after name is
            // confirmed so `capture_into_profile` sees `active_profile.is_none()`
            // and links the new one. On Esc this never runs.
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
        KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => input.delete_word(),
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

/// Apply queued status-feed events to `app.status`. Surfaces a new-incident cue
/// (toast on the Status tab, tab-activity color elsewhere) and a manual-refresh
/// failure toast. Manual refresh always clears `fetching`.
fn drain_status_events(app: &mut App) {
    while let Ok(ev) = app.status_events.try_recv() {
        match ev {
            StatusEvent::Fetched {
                incidents,
                fetched_at_ms,
            } => {
                let was_manual = app.status.fetching;
                apply_status_incidents(app, incidents, fetched_at_ms, false, was_manual);
            }
            StatusEvent::Cached {
                incidents,
                fetched_at_ms,
            } => {
                // A cache fallback after a manual refresh means the live fetch
                // failed — surface that as a danger toast.
                let was_manual = app.status.fetching;
                apply_status_incidents(app, incidents, fetched_at_ms, true, false);
                if was_manual {
                    app.status.fetching = false;
                    app.toast(ToastKind::Danger, "status refresh failed — showing cached");
                }
            }
            StatusEvent::Failed(msg) => {
                let was_manual = app.status.fetching;
                app.status.fetching = false;
                // Keep an error only while nothing is loaded; avoid toast spam on
                // the steady-state retry loop. A manual failure does toast.
                if app.status.incidents.is_empty() {
                    app.status.error = Some(msg);
                }
                if was_manual {
                    app.toast(ToastKind::Danger, "status refresh failed");
                }
            }
        }
    }
}

/// Replace the incident list, clear the spinner, clamp the cursor, and run the
/// new-incident signal. `cached` marks the data as a cache load / fallback.
/// `manual` is true when this `Fetched` answers a manual refresh (toasts there).
fn apply_status_incidents(
    app: &mut App,
    incidents: Vec<Incident>,
    fetched_at_ms: u64,
    cached: bool,
    manual: bool,
) {
    let prev_selected_id = app.status.selected().map(|i| i.id.clone());

    app.status.fetching = false;
    app.status.cached = cached;
    app.status.fetched_at_ms = Some(fetched_at_ms);
    if !cached {
        app.status.error = None;
    }

    // New-incident signal: compare the newest id to the last one we signalled.
    let newest_id = incidents.first().map(|i| i.id.clone());
    if let Some(newest) = &newest_id
        && app.status.seen_latest.as_ref() != Some(newest)
    {
        let is_initial = app.status.seen_latest.is_none();
        // Only signal genuinely new incidents, never the initial load.
        if !is_initial && let Some(incident) = incidents.first() {
            let severity = if incident_is_active(incident) {
                ToastKind::Warning
            } else {
                ToastKind::Info
            };
            if app.tab == Tab::Status {
                let title = truncate_chars(&incident.title, 40);
                app.toast(severity, format!("new incident · {title}"));
            } else {
                app.set_tab_activity(Tab::Status, severity);
            }
        }
        app.status.seen_latest = newest_id.clone();
    }
    let _ = manual; // a fresh `Fetched` needs no extra toast beyond the new-incident cue.

    app.status.incidents = incidents;

    // Clamp the cursor and reset the detail scroll if the selection changed.
    if app.status.incidents.is_empty() {
        app.status.cursor = 0;
    } else if app.status.cursor >= app.status.incidents.len() {
        app.status.cursor = app.status.incidents.len() - 1;
    }
    if app.status.selected().map(|i| i.id.clone()) != prev_selected_id {
        app.status.detail_scroll = 0;
    }
}

/// Truncate a string to `max` chars, appending `…` when cut.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

pub(crate) fn on_tick(app: &mut App) {
    app.tick_count = app.tick_count.wrapping_add(1);

    // Drain update check result (worker emits at most one event).
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

    drain_op_results(app);
    drain_status_events(app);

    if app.reload_if_state_changed() {
        app.clamp_profile_cursor();
    }
    app.apply_usage();

    // Arm opted-in profiles with no live 5h window. Skip while bootstrap is
    // running — it's mid-`refresh_all` and rotated tokens must land first.
    let pending: Vec<String> = if app.bootstrap_active.load(Ordering::SeqCst) {
        Vec::new()
    } else {
        let candidates = oauth::windowless_auto_start_candidates(&app.config, &app.usage_store);
        let raw: Vec<String> = app
            .pending_auto_start
            .lock()
            .map(|mut g| {
                g.extend(candidates);
                g.drain().collect()
            })
            .unwrap_or_default();
        // Debounce live-session candidates.
        ready_auto_start(app, raw)
    };
    for name in pending {
        if !is_idle(&app.activity, &name) {
            continue;
        }
        let config = Arc::clone(&app.config);
        let tokens = Arc::clone(&app.usage_tokens);
        let refetch = Arc::clone(&app.refetch_queue);
        let work_name = name.clone();
        let work_activity = Arc::clone(&app.activity);
        let work_sender = app.op_sender.clone();
        spawn_profile_worker(
            name,
            ActivityKind::AutoStarting,
            "auto-start worker panicked",
            Arc::clone(&app.activity),
            app.op_sender.clone(),
            move || {
                let _ = oauth::start_window(
                    &config,
                    &work_name,
                    Some(&tokens),
                    Some(&refetch),
                    Some(&work_activity),
                    &work_sender,
                );
            },
        );
    }

    // Drain scheduler auto-switch decisions; skip non-idle targets.
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
        app.set_tab_activity(Tab::Overview, ToastKind::Warning);
        perform_switch(app, &name);
    }

    drain_pending_switch_off(app);

    drain_startup_signals(app);
    maybe_spawn_bootstrap(app);

    poll_credentials_divergence(app);

    update_banner(app);
    app.prune_toasts();
}

/// Recompute the sticky banner from current app state. Called every tick.
/// Winner ordering: `DANGER` > `WARNING`; ties broken by whichever condition
/// is checked first. One condition per `if` block in priority order.
fn update_banner(app: &mut App) {
    // All accounts switched off because the fallback chain is exhausted.
    // Condition: profiles exist but none is active (set by `perform_switch_off`).
    let cfg = app.config();
    let all_spent = !cfg.profiles.is_empty() && cfg.state.active_profile.is_none();
    drop(cfg);

    app.banner = if all_spent {
        Some(Banner {
            severity: BannerSeverity::Danger,
            message: "all accounts spent · switch to a profile to resume".to_string(),
        })
    } else if app.compact {
        // Terminal too small for the full layout. Lower severity than the
        // all-spent danger above, so danger wins the tie.
        Some(Banner {
            severity: BannerSeverity::Warning,
            message: "terminal too small · enlarge for full layout".to_string(),
        })
    } else {
        None
    };
}

/// Drain worker op results: clear activity slots, toast errors/successes,
/// rebuild token snapshot on Refreshing/AutoStarting success.
fn drain_op_results(app: &mut App) {
    let mut needs_token_snapshot_rebuild = false;
    let mut auto_started_names: Vec<String> = Vec::new();
    while let Ok(OpResult {
        name,
        kind,
        outcome,
    }) = app.op_results.try_recv()
    {
        // Only clear when the slot still reflects this op's kind.
        // Invariant: `Fetching` is NEVER sent via OpResult (managed by the
        // join loop directly); `Switching` never arrives either (UI-thread relink).
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
                    app.set_tab_activity(Tab::Usage, ToastKind::Info);
                }
                ActivityKind::Refreshing => {
                    needs_token_snapshot_rebuild = true;
                    app.toast(ToastKind::Info, format!("rotated token for '{name}'"));
                    app.set_tab_activity(Tab::Usage, ToastKind::Info);
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
                // Route the failure to the relevant tab.
                let failure_tab = match kind {
                    ActivityKind::Refreshing | ActivityKind::AutoStarting => Tab::Usage,
                    ActivityKind::Switching | ActivityKind::Starting => Tab::Overview,
                    _ => Tab::Overview,
                };
                app.set_tab_activity(failure_tab, ToastKind::Danger);
            }
        }
    }
    if needs_token_snapshot_rebuild {
        app.refresh_tokens();
    }
    // Optimistically mark kicked windows open before the re-fetch: /usage can
    // rate-limit for minutes after a kick, and until a live body lands the
    // usage tab + auto-start scan would still see the stale windowless data.
    for name in &auto_started_names {
        crate::usage::mark_window_open(&app.usage_store, name, crate::usage::now_epoch_secs());
    }
    // Route auto-start re-fetches through RefetchQueue (not all-profile refresh).
    if !auto_started_names.is_empty()
        && let Ok(mut q) = app.refetch_queue.lock()
    {
        q.extend(auto_started_names.drain(..));
    }
}

/// Drain the wrap-off flag. Only drain when no modal is open — `perform_switch_off`
/// may raise a Divergence prompt; consuming the flag while one is open would
/// let the scheduler re-set it and stack duplicates.
fn drain_pending_switch_off(app: &mut App) {
    if !app.modals.is_empty() {
        return;
    }
    let switch_off_pending = app
        .pending_switch_off
        .lock()
        .map(|mut g| std::mem::replace(&mut *g, false))
        .unwrap_or(false);
    if switch_off_pending {
        perform_switch_off(app);
    }
}

/// Drain startup signals from reconcile/bootstrap workers.
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

/// Spawn bootstrap once reconcile is done and no modal is open.
fn maybe_spawn_bootstrap(app: &mut App) {
    if app.bootstrap_started || !app.reconcile_done || !app.modals.is_empty() {
        return;
    }
    app.bootstrap_started = true;
    app.bootstrap_active.store(true, Ordering::SeqCst);
    app.spawn_bootstrap();
}

/// 1Hz check: push Divergence modal if `.credentials.json` no longer matches
/// the active profile's stored creds. Skips when a modal is already open.
fn poll_credentials_divergence(app: &mut App) {
    const POLL_INTERVAL: Duration = Duration::from_secs(1);

    if app.last_divergence_check.elapsed() < POLL_INTERVAL {
        return;
    }
    app.last_divergence_check = Instant::now();

    if !app.modals.is_empty() {
        return;
    }
    let Some(active) = app
        .config()
        .state
        .active_profile
        .as_deref()
        .map(str::to_string)
    else {
        return;
    };
    if !matches!(
        classify_credentials_link(&active).ok(),
        Some(LinkState::Diverged)
    ) {
        return;
    }
    // First login on a credential-less profile: adopt silently, don't prompt.
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

/// Snapshot active credentials and detach the symlink. After shutdown, external
/// writes to `.credentials.json` land in the standalone file, not the profile.
pub(crate) fn shutdown(app: &mut App) -> Result<()> {
    {
        let mut cfg = app.config();
        let _ = snapshot_active_credentials(&mut cfg);
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

    use super::{InputState, parse_threshold};

    #[test]
    fn delete_word_removes_run_left_of_caret() {
        let mut input = InputState::new("foo bar");
        input.delete_word();
        assert_eq!(input.value, "foo ");
        input.delete_word();
        assert_eq!(input.value, "");
    }

    #[test]
    fn delete_word_respects_caret_position() {
        let mut input = InputState::new("foo bar");
        input.home(); // caret at start — nothing to the left
        input.delete_word();
        assert_eq!(input.value, "foo bar");
    }

    #[test]
    fn parse_threshold_accepts_in_range_only() {
        assert_eq!(parse_threshold("0"), Some(0.0));
        assert_eq!(parse_threshold("100"), Some(100.0));
        assert_eq!(parse_threshold("73.5"), Some(73.5));
        assert!(parse_threshold("150").is_none());
        assert!(parse_threshold("-1").is_none());
        assert!(parse_threshold("abc").is_none());
        assert!(parse_threshold("").is_none());
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

    use super::{AUTO_START_LIVE_SESSION_DEBOUNCE_MS, debounce_live_candidates};
    use std::collections::HashSet;

    #[test]
    fn debounce_passes_non_live_candidate_through() {
        let mut since = HashMap::new();
        let live = HashSet::new();
        let ready = debounce_live_candidates(&mut since, vec!["bg".to_string()], &live, 1_000_000);
        assert_eq!(ready, vec!["bg".to_string()]);
        assert!(since.is_empty(), "no timer for a non-live candidate");
    }

    #[test]
    fn debounce_holds_live_candidate_on_first_sight() {
        let mut since = HashMap::new();
        let live = HashSet::from(["cc".to_string()]);
        let ready = debounce_live_candidates(&mut since, vec!["cc".to_string()], &live, 1_000_000);
        assert!(
            ready.is_empty(),
            "live candidate must wait out the debounce"
        );
        assert_eq!(since.get("cc"), Some(&1_000_000), "first-seen stamped");
    }

    #[test]
    fn debounce_arms_live_candidate_after_window() {
        let now = 5_000_000;
        let mut since =
            HashMap::from([("cc".to_string(), now - AUTO_START_LIVE_SESSION_DEBOUNCE_MS)]);
        let live = HashSet::from(["cc".to_string()]);
        let ready = debounce_live_candidates(&mut since, vec!["cc".to_string()], &live, now);
        assert_eq!(ready, vec!["cc".to_string()]);
    }

    #[test]
    fn debounce_prunes_stale_timers() {
        let mut since = HashMap::from([("gone".to_string(), 1)]);
        let live = HashSet::new();
        let ready = debounce_live_candidates(&mut since, Vec::new(), &live, 9_000_000);
        assert!(ready.is_empty());
        assert!(since.is_empty(), "stale timer pruned");
    }

    // ── compact mode ─────────────────────────────────────────────────────────

    use super::App;

    fn bare_app() -> App {
        use crate::profile::{AppConfig, AppState};
        App::new(AppConfig {
            state: AppState::default(),
            profiles: Vec::new(),
        })
    }

    /// Compact entry (< 14 rows) sets the flag and emits no toast; the warning
    /// banner is driven from the flag in `update_banner` instead.
    #[test]
    fn compact_entry_sets_flag_no_toast() {
        let mut app = bare_app();
        app.update_compact(13);
        assert!(app.compact);
        assert!(app.toasts.is_empty(), "compact must not fire a toast");
    }

    /// Compact recomputes a WARNING banner with the contract's exact message.
    #[test]
    fn compact_yields_warning_banner() {
        use super::{BannerSeverity, update_banner};
        let mut app = bare_app();
        app.update_compact(13);
        update_banner(&mut app);
        let banner = app.banner.as_ref().expect("compact banner present");
        assert_eq!(banner.severity, BannerSeverity::Warning);
        assert_eq!(
            banner.message,
            "terminal too small · enlarge for full layout"
        );
    }

    /// Growing back above threshold clears the flag and self-clears the banner.
    #[test]
    fn compact_exit_clears_banner() {
        use super::update_banner;
        let mut app = bare_app();
        app.update_compact(13);
        update_banner(&mut app);
        assert!(app.banner.is_some());
        app.update_compact(14);
        update_banner(&mut app);
        assert!(!app.compact);
        assert!(app.banner.is_none(), "banner self-clears on resize");
    }

    /// Re-entering compact after a return to normal re-shows the banner; never a toast.
    #[test]
    fn compact_rearm_after_exit() {
        use super::update_banner;
        let mut app = bare_app();
        app.update_compact(13);
        app.update_compact(14); // exit
        app.update_compact(13); // re-enter
        update_banner(&mut app);
        assert!(app.compact);
        assert!(app.toasts.is_empty(), "compact must not fire a toast");
        assert!(app.banner.is_some());
    }

    // ── global config tab ────────────────────────────────────────────────────

    use super::theme::{self, Tier};
    use super::{GLOBAL_CONFIG_ROWS, KeyCode, KeyEvent, KeyModifiers, Tab};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// The live-swap holder round-trips so a re-selection re-renders in the new
    /// tier without a restart.
    #[test]
    fn theme_set_tier_round_trips() {
        theme::set_tier(Tier::Full);
        assert_eq!(theme::tier(), Tier::Full);
        theme::set_tier(Tier::Compatible);
        assert_eq!(theme::tier(), Tier::Compatible);
        theme::set_tier(Tier::Full);
        assert_eq!(theme::tier(), Tier::Full);
    }

    /// ↑↓ on the Config tab wraps through the global rows in both directions.
    #[test]
    fn global_config_cursor_wraps() {
        let mut app = bare_app();
        app.tab = Tab::Config;
        let last = GLOBAL_CONFIG_ROWS.len() - 1;

        assert_eq!(app.global_config_cursor, 0);
        super::handle_global_config_key(&mut app, key(KeyCode::Up));
        assert_eq!(
            app.global_config_cursor, last,
            "Up from first wraps to last"
        );
        super::handle_global_config_key(&mut app, key(KeyCode::Down));
        assert_eq!(app.global_config_cursor, 0, "Down from last wraps to first");
    }

    /// Every Config-tab row maps to a non-empty action-menu entry, so `a` is
    /// never a dead key on this tab.
    #[test]
    fn global_config_rows_have_actions() {
        for (i, _row) in GLOBAL_CONFIG_ROWS.iter().enumerate() {
            let mut app = bare_app();
            app.tab = Tab::Config;
            app.global_config_cursor = i;
            let menu = super::build_action_menu(&app);
            assert!(
                !menu.items.is_empty(),
                "row {i} must surface at least one action"
            );
        }
    }
}
