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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    LinkState, classify_credentials_link, credentials_diverged, detach_credentials_link,
    force_link_profile_credentials, force_snapshot_active_credentials, link_profile_credentials,
    read_claude_credentials, snapshot_active_credentials,
};
use crate::fallback::{DEFAULT_THRESHOLD, auto_switch_if_needed, threshold_for};
use crate::lock::with_state_lock;
use crate::oauth;
use crate::profile::{
    AppConfig, Profile, app_state_mtime, load_config, save_app_state, save_profile,
};
use crate::usage::{
    ActivityFlag, LastFetchedAt, LastStable, NextRefreshAt, PendingAutoStart, StatusStore,
    TokenEntry, TokenList, UsageStore, default_fallback_threshold, fetch_all_into, now_ms,
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
    CaptureConflict(Box<CaptureSnapshot>),
    Switch(String),
    /// Confirm step before discarding CC's freshly-written credentials and
    /// re-linking the live path to the named profile's stored creds.
    DiscardDivergence(String),
}

#[derive(Debug, Clone)]
pub(crate) struct CaptureNameForm {
    pub(crate) snapshot: Box<CaptureSnapshot>,
    pub(crate) input: InputState,
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
    pub(crate) activity: ActivityFlag,
    pub(crate) next_refresh_at: NextRefreshAt,
    pub(crate) last_fetched: LastFetchedAt,
    pub(crate) last_stable: LastStable,
    pub(crate) pending_auto_start: PendingAutoStart,

    pub(crate) screen: Screen,
    pub(crate) modals: Vec<Modal>,

    pub(crate) main_cursor: usize,
    pub(crate) chain_cursor: usize,

    pub(crate) toasts: VecDeque<Toast>,

    pub(crate) last_state_mtime: Option<SystemTime>,
    pub(crate) started_at: Instant,
    pub(crate) quit: bool,
    /// Last time the 1Hz divergence poll ran. Re-checks whether
    /// `~/.claude/.credentials.json` still points at the active profile and
    /// pushes a Divergence modal when CC has overwritten the symlink
    /// (typically by `/login`). Defers behind any open modal.
    pub(crate) last_divergence_check: Instant,
}

impl App {
    pub(crate) fn new(config: AppConfig) -> Self {
        let usage_store: UsageStore = Arc::new(Mutex::new(HashMap::new()));
        let usage_status: StatusStore = Arc::new(Mutex::new(HashMap::new()));
        let usage_tokens: TokenList = Arc::new(Mutex::new(collect_tokens(&config.profiles)));
        let activity: ActivityFlag = Arc::new(AtomicBool::new(false));
        let next_refresh_at: NextRefreshAt = Arc::new(AtomicU64::new(now_ms() + 30_000));
        let last_fetched: LastFetchedAt = Arc::new(Mutex::new(HashMap::new()));
        let last_stable: LastStable = Arc::new(Mutex::new(HashMap::new()));
        let pending_auto_start: PendingAutoStart = Arc::new(Mutex::new(HashSet::new()));

        Self {
            config: Arc::new(Mutex::new(config)),
            usage_store,
            usage_status,
            usage_tokens,
            activity,
            next_refresh_at,
            last_fetched,
            last_stable,
            pending_auto_start,
            screen: Screen::Overview,
            modals: Vec::new(),
            main_cursor: 0,
            chain_cursor: 0,
            toasts: VecDeque::new(),
            last_state_mtime: app_state_mtime(),
            started_at: Instant::now(),
            quit: false,
            last_divergence_check: Instant::now(),
        }
    }

    /// Lock the shared AppConfig. Holds the lock for the lifetime of the
    /// returned guard. Order: AppConfig mutex outer, `with_state_lock` inner;
    /// the inner is taken by the actions that mutate disk state.
    pub(crate) fn config(&self) -> MutexGuard<'_, AppConfig> {
        self.config.lock().expect("config mutex poisoned")
    }

    /// Kick off the background usage refresher. Runs once after startup
    /// reconciliation completes so the active profile's identity is settled
    /// before we rotate any refresh tokens.
    pub(crate) fn bootstrap_usage(&mut self) {
        // Re-establish the credentials symlink that the previous shutdown
        // replaced with a plain file. Without this, in-session Claude Code
        // refreshes write to a standalone file instead of the profile.
        let active = self.config().state.active_profile.clone();
        if let Some(active) = active {
            let _ = link_profile_credentials(&active);
        }

        // Refresh every profile's OAuth token pair — Claude Code does the
        // same thing silently on launch. Rotates and persists the new pair
        // so the initial usage fetch below uses fresh access tokens.
        {
            let mut cfg = self.config();
            let _ = oauth::refresh_all(&mut cfg);
        }
        self.refresh_tokens();

        let snapshot = self
            .usage_tokens
            .lock()
            .expect("usage_tokens mutex poisoned")
            .clone();
        fetch_all_into(
            &snapshot,
            &self.usage_store,
            &self.usage_status,
            &self.activity,
            &self.last_fetched,
            &self.last_stable,
            &self.pending_auto_start,
        );

        let started = {
            let mut cfg = self.config();
            oauth::auto_start_windows(&mut cfg, &self.usage_store)
        };
        if !started.is_empty() {
            let retry: Vec<TokenEntry> = collect_tokens(&self.config().profiles)
                .into_iter()
                .filter(|e| started.contains(&e.name))
                .collect();
            fetch_all_into(
                &retry,
                &self.usage_store,
                &self.usage_status,
                &self.activity,
                &self.last_fetched,
                &self.last_stable,
                &self.pending_auto_start,
            );
            *self
                .usage_tokens
                .lock()
                .expect("usage_tokens mutex poisoned") = collect_tokens(&self.config().profiles);
        }

        spawn_refresher(
            Arc::clone(&self.usage_tokens),
            Arc::clone(&self.usage_store),
            Arc::clone(&self.usage_status),
            Arc::clone(&self.activity),
            Arc::clone(&self.next_refresh_at),
            Arc::clone(&self.last_fetched),
            Arc::clone(&self.last_stable),
            Arc::clone(&self.pending_auto_start),
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
        let info_map = self.usage_store.lock().ok();
        let status_map = self.usage_status.lock().ok();
        let mut cfg = self.config();
        for p in &mut cfg.profiles {
            p.usage = info_map.as_ref().and_then(|s| s.get(&p.name)).cloned();
            p.fetch_status = status_map.as_ref().and_then(|s| s.get(&p.name).copied());
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
        // Manual refresh bypasses the cache rule. Clearing `last_fetched`
        // forces every entry to be due, both for the immediate fetch we kick
        // off here and for the refresher's next tick.
        if let Ok(mut lf) = self.last_fetched.lock() {
            lf.clear();
        }
        let snapshot = self
            .usage_tokens
            .lock()
            .expect("usage_tokens mutex poisoned")
            .clone();
        self.next_refresh_at
            .store(now_ms() + 30_000, Ordering::Relaxed);
        let store = Arc::clone(&self.usage_store);
        let status = Arc::clone(&self.usage_status);
        let activity = Arc::clone(&self.activity);
        let last_fetched = Arc::clone(&self.last_fetched);
        let last_stable = Arc::clone(&self.last_stable);
        let pending_auto_start = Arc::clone(&self.pending_auto_start);
        std::thread::spawn(move || {
            fetch_all_into(
                &snapshot,
                &store,
                &status,
                &activity,
                &last_fetched,
                &last_stable,
                &pending_auto_start,
            );
        });
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
                auto_start: p.auto_start,
            })
        })
        .collect()
}

// ── Startup reconciliation ────────────────────────────────────────────────────

/// Resolve the gap between the live `~/.claude/.credentials.json` and the
/// active profile's stored credentials. Most of the time this is a silent
/// snapshot. When tokens diverge — usually because Claude Code was used to
/// sign into a different account between sessions — push a modal to ask the
/// user before overwriting the stored identity.
pub(crate) fn reconcile_startup(app: &mut App) {
    let Some(active) = app.config().state.active_profile.clone() else {
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
        return;
    }

    // Tokens differ — can't tell from bytes alone whether CC silently
    // refreshed (rotating the stored chain) or did a fresh `/login` on a
    // separate chain. Probe by attempting to refresh the stored
    // refresh_token: Anthropic rotates on every refresh, so the call fails
    // iff CC already used it. Failure → live is the legit continuation,
    // snapshot silently. Success → stored chain is still alive, CC's
    // tokens come from a relog; persist the rotated pair (the old one is
    // now invalid server-side anyway) and prompt the user.
    let stored_refresh = app
        .config()
        .find(&active)
        .and_then(|p| p.refresh_token().map(str::to_string));
    if let Some(rt) = stored_refresh {
        match oauth::refresh(&rt) {
            Err(_) => {
                let mut cfg = app.config();
                let _ = force_snapshot_active_credentials(&mut cfg);
                return;
            }
            Ok(tok) => {
                let mut cfg = app.config();
                let _ = oauth::apply_rotated_tokens(&mut cfg, &active, tok);
            }
        }
    }

    app.modals
        .push(Modal::Divergence(DivergenceForm { active, cursor: 0 }));
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
        MainItemKind::CaptureCredentials => begin_capture(app),
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

fn perform_switch(app: &mut App, name: &str) {
    let result = {
        let mut cfg = app.config();
        let _ = oauth::refresh_all(&mut cfg);
        switch_profile(&mut cfg, name)
    };
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

fn begin_capture(app: &mut App) {
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
            on_confirm: ConfirmAction::CaptureConflict(Box::new(snapshot)),
        }));
        return;
    }
    app.modals.push(Modal::CaptureName(CaptureNameForm {
        snapshot: Box::new(snapshot),
        input: InputState::new(""),
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
        ConfirmAction::CaptureConflict(snapshot) => {
            app.modals.push(Modal::CaptureName(CaptureNameForm {
                snapshot,
                input: InputState::new(""),
            }));
        }
        ConfirmAction::Switch(name) => perform_switch(app, &name),
        ConfirmAction::DiscardDivergence(name) => run_discard_divergence(app, &name),
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
            let _ = detach_credentials_link();
            {
                let mut cfg = app.config();
                cfg.state.active_profile = None;
                let _ = save_app_state(&cfg.state);
            }
            begin_capture(app);
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
    if app.reload_if_state_changed() {
        app.clamp_main_cursor();
    }
    app.apply_usage();

    // Drain the auto-start queue the refresher fills when a successful fetch
    // shows no live 5h window. Mutation stays on the main thread so we keep
    // reusing the existing `&mut AppConfig` flow. `auto_start_named` enforces
    // its own per-profile cooldown, so a duplicate insert is a no-op.
    let pending: Vec<String> = {
        let mut p = app
            .pending_auto_start
            .lock()
            .expect("pending_auto_start mutex poisoned");
        let v: Vec<String> = p.iter().cloned().collect();
        p.clear();
        v
    };
    let mut started = Vec::new();
    for name in pending {
        let kicked = {
            let mut cfg = app.config();
            oauth::auto_start_named(&mut cfg, &name)
        };
        if kicked {
            started.push(name);
        }
    }
    if !started.is_empty() {
        app.refresh_tokens();
        app.manual_refresh();
        let body = if started.len() == 1 {
            format!("auto-started usage window for '{}'", started[0])
        } else {
            format!("auto-started {} usage windows", started.len())
        };
        app.toast(ToastKind::Info, body);
    }

    let switched = {
        let mut cfg = app.config();
        auto_switch_if_needed(&mut cfg).ok().flatten()
    };
    if let Some(target) = switched {
        app.refresh_tokens();
        app.last_state_mtime = app_state_mtime();
        app.toast(ToastKind::Warning, format!("auto-switched to '{target}'"));
    }

    poll_credentials_divergence(app);

    app.prune_toasts();
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
    app.modals
        .push(Modal::Divergence(DivergenceForm { active, cursor: 0 }));
}

// ── Shutdown ──────────────────────────────────────────────────────────────────

/// Persist whatever Claude Code wrote during this session, then replace the
/// symlink with a plain copy. After shutdown any external write to
/// ~/.claude/.credentials.json lands in that standalone file instead of
/// mutating the active profile's storage through the link.
pub(crate) fn shutdown(app: &mut App) -> Result<()> {
    {
        let mut cfg = app.config();
        let _ = snapshot_active_credentials(&mut cfg);
    }
    let _ = detach_credentials_link();
    Ok(())
}
