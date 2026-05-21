//! Application state, keymap, and tick logic.
//!
//! Layout invariants:
//!   - The main menu is a single list with profile rows followed by a small
//!     set of action rows. Indices into this list live in `main_cursor`.
//!   - The chain editor is a second screen with its own cursor.
//!   - Modals stack: the top of `modals` owns input; events fall through to
//!     the screen below only when the stack is empty.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::actions::{
    CaptureSnapshot, capture_into_profile, capture_snapshot, create_blank_profile, delete_profile,
    edit_profile_endpoint, find_matching_oauth_profile, rename_profile, reorder_profile,
    switch_profile, validate_profile_name,
};
use crate::claude::{
    credentials_diverged, detach_credentials_link, link_profile_credentials,
    read_claude_credentials, snapshot_active_credentials,
};
use crate::fallback::{DEFAULT_THRESHOLD, auto_switch_if_needed, threshold_for};
use crate::lock::with_state_lock;
use crate::oauth;
use crate::profile::{
    AppConfig, Profile, app_state_mtime, load_config, save_app_state, save_profile,
};
use crate::usage::{
    ActivityFlag, HistoryStore, NextRefreshAt, REFRESH_INTERVAL, StatusStore, TokenList,
    UsageStore, fetch_all_into, now_ms, spawn_refresher,
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
}

#[derive(Debug, Clone)]
pub(crate) struct CaptureNameForm {
    pub(crate) snapshot: Box<CaptureSnapshot>,
    pub(crate) input: InputState,
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
    Switch,
    Details,
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
    /// Step 1 of startup reconciliation. Step 2 morphs into a Confirm modal.
    ReconcileKeep {
        active: String,
        choice: bool,
    },
    /// Step 2 — "capture current credentials as a new profile?"
    ReconcileCaptureAsk {
        choice: bool,
    },
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

// ── Filter ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct FilterState {
    pub(crate) input: InputState,
    /// True while keystrokes feed the input; false once Enter pins the term
    /// so list nav keys work.
    pub(crate) focused: bool,
}

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
    NewProfile,
    CaptureCredentials,
    OpenChain,
}

// ── App ───────────────────────────────────────────────────────────────────────

pub(crate) struct App {
    pub(crate) config: AppConfig,

    pub(crate) usage_store: UsageStore,
    pub(crate) usage_status: StatusStore,
    pub(crate) usage_history: HistoryStore,
    pub(crate) usage_tokens: TokenList,
    pub(crate) activity: ActivityFlag,
    pub(crate) next_refresh_at: NextRefreshAt,

    pub(crate) screen: Screen,
    pub(crate) modals: Vec<Modal>,

    pub(crate) main_cursor: usize,
    pub(crate) chain_cursor: usize,
    pub(crate) filter: Option<FilterState>,

    pub(crate) toasts: VecDeque<Toast>,

    pub(crate) last_state_mtime: Option<SystemTime>,
    pub(crate) started_at: Instant,
    pub(crate) quit: bool,
    /// Set true while the background usage refresher has work in flight. When
    /// it drops false on the next tick we run auto-start, so the kick lands
    /// after a refresh cycle confirmed which profiles have no 5h window.
    pub(crate) was_busy: bool,
    /// True for the busy→idle edge immediately after we fired auto-start; the
    /// subsequent manual_refresh flips activity again, so we'd otherwise
    /// re-enter the auto-start path on the next idle edge.
    pub(crate) just_auto_started: bool,
}

impl App {
    pub(crate) fn new(config: AppConfig) -> Self {
        let usage_store: UsageStore = Arc::new(Mutex::new(HashMap::new()));
        let usage_status: StatusStore = Arc::new(Mutex::new(HashMap::new()));
        let usage_history: HistoryStore = Arc::new(Mutex::new(HashMap::new()));
        let usage_tokens: TokenList = Arc::new(Mutex::new(collect_tokens(&config.profiles)));
        let activity: ActivityFlag = Arc::new(AtomicBool::new(false));
        let next_refresh_at: NextRefreshAt = Arc::new(AtomicU64::new(
            now_ms() + REFRESH_INTERVAL.as_millis() as u64,
        ));

        Self {
            config,
            usage_store,
            usage_status,
            usage_history,
            usage_tokens,
            activity,
            next_refresh_at,
            screen: Screen::Overview,
            modals: Vec::new(),
            main_cursor: 0,
            chain_cursor: 0,
            filter: None,
            toasts: VecDeque::new(),
            last_state_mtime: app_state_mtime(),
            started_at: Instant::now(),
            quit: false,
            was_busy: false,
            just_auto_started: false,
        }
    }

    /// Kick off the background usage refresher. Runs once after startup
    /// reconciliation completes so the active profile's identity is settled
    /// before we rotate any refresh tokens.
    pub(crate) fn bootstrap_usage(&mut self) {
        // Re-establish the credentials symlink that the previous shutdown
        // replaced with a plain file. Without this, in-session Claude Code
        // refreshes write to a standalone file instead of the profile.
        if let Some(active) = self.config.state.active_profile.clone() {
            let _ = link_profile_credentials(&active);
        }

        // Refresh every profile's OAuth token pair — Claude Code does the
        // same thing silently on launch. Rotates and persists the new pair
        // so the initial usage fetch below uses fresh access tokens.
        let _ = oauth::refresh_all(&mut self.config);
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
            &self.usage_history,
            &self.activity,
        );

        let started = oauth::auto_start_windows(&mut self.config, &self.usage_store);
        if !started.is_empty() {
            let retry: Vec<(String, String)> = collect_tokens(&self.config.profiles)
                .into_iter()
                .filter(|(name, _)| started.contains(name))
                .collect();
            fetch_all_into(
                &retry,
                &self.usage_store,
                &self.usage_status,
                &self.usage_history,
                &self.activity,
            );
            *self
                .usage_tokens
                .lock()
                .expect("usage_tokens mutex poisoned") = collect_tokens(&self.config.profiles);
        }

        spawn_refresher(
            Arc::clone(&self.usage_tokens),
            Arc::clone(&self.usage_store),
            Arc::clone(&self.usage_status),
            Arc::clone(&self.usage_history),
            Arc::clone(&self.activity),
            Arc::clone(&self.next_refresh_at),
        );

        self.apply_usage();
        if let Ok(Some(target)) = auto_switch_if_needed(&mut self.config) {
            self.toast(ToastKind::Warning, format!("auto-switched to '{target}'"));
        }
    }

    pub(crate) fn apply_usage(&mut self) {
        let info_map = self.usage_store.lock().ok();
        let status_map = self.usage_status.lock().ok();
        for p in &mut self.config.profiles {
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
            self.config = fresh;
            self.last_state_mtime = current;
            *self
                .usage_tokens
                .lock()
                .expect("usage_tokens mutex poisoned") = collect_tokens(&self.config.profiles);
            true
        } else {
            false
        }
    }

    pub(crate) fn refresh_tokens(&self) {
        *self
            .usage_tokens
            .lock()
            .expect("usage_tokens mutex poisoned") = collect_tokens(&self.config.profiles);
    }

    pub(crate) fn manual_refresh(&self) {
        // Pull current tokens fresh; tick the refresher's deadline forward so
        // the background loop doesn't immediately duplicate this work.
        let snapshot = self
            .usage_tokens
            .lock()
            .expect("usage_tokens mutex poisoned")
            .clone();
        self.next_refresh_at.store(
            now_ms() + REFRESH_INTERVAL.as_millis() as u64,
            Ordering::Relaxed,
        );
        let store = Arc::clone(&self.usage_store);
        let status = Arc::clone(&self.usage_status);
        let history = Arc::clone(&self.usage_history);
        let activity = Arc::clone(&self.activity);
        std::thread::spawn(move || {
            fetch_all_into(&snapshot, &store, &status, &history, &activity);
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

    /// Profile indices visible under the current filter, preserving order.
    pub(crate) fn visible_profile_indices(&self) -> Vec<usize> {
        let filter = self
            .filter
            .as_ref()
            .map(|f| f.input.value.to_lowercase())
            .unwrap_or_default();
        if filter.is_empty() {
            return (0..self.config.profiles.len()).collect();
        }
        self.config
            .profiles
            .iter()
            .enumerate()
            .filter_map(|(i, p)| p.name.to_lowercase().contains(&filter).then_some(i))
            .collect()
    }

    pub(crate) fn main_items(&self) -> Vec<MainItemKind> {
        let mut items: Vec<MainItemKind> = self
            .visible_profile_indices()
            .into_iter()
            .map(MainItemKind::Profile)
            .collect();
        // Action rows are suppressed while an active filter is narrowing results.
        let filter_active = self
            .filter
            .as_ref()
            .is_some_and(|f| !f.input.value.is_empty());
        if !filter_active {
            items.push(MainItemKind::NewProfile);
            items.push(MainItemKind::CaptureCredentials);
            items.push(MainItemKind::OpenChain);
        }
        items
    }

    pub(crate) fn clamp_main_cursor(&mut self) {
        let len = self.main_items().len();
        if len == 0 {
            self.main_cursor = 0;
        } else if self.main_cursor >= len {
            self.main_cursor = len - 1;
        }
    }

    pub(crate) fn current_main_item(&self) -> Option<MainItemKind> {
        self.main_items().get(self.main_cursor).copied()
    }
}

// ── Token snapshot ────────────────────────────────────────────────────────────

pub(crate) fn collect_tokens(profiles: &[Profile]) -> Vec<(String, String)> {
    profiles
        .iter()
        .filter_map(|p| {
            let token = p
                .credentials
                .as_ref()?
                .claude_ai_oauth
                .as_ref()?
                .access_token
                .clone();
            Some((p.name.clone(), token))
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
    let Some(active) = app.config.state.active_profile.clone() else {
        return;
    };

    // Read the live credentials under the same lock that mutators use, so a
    // concurrent clauth process mid-rotation can't expose a torn snapshot.
    let live = with_state_lock(|| Ok(read_claude_credentials().ok().flatten()))
        .ok()
        .flatten();
    let stored = app
        .config
        .find(&active)
        .and_then(|p| p.credentials.as_ref());

    if !credentials_diverged(stored, live.as_ref()) {
        let _ = snapshot_active_credentials(&mut app.config);
        return;
    }

    app.modals.push(Modal::ReconcileKeep {
        active,
        choice: true,
    });
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
    // Filter input mode steals most of the keymap.
    if let Some(filter) = app.filter.as_mut()
        && filter.focused
    {
        match key.code {
            KeyCode::Enter => {
                if filter.input.value.is_empty() {
                    app.filter = None;
                } else {
                    filter.focused = false;
                }
                app.main_cursor = 0;
            }
            KeyCode::Esc => {
                app.filter = None;
                app.main_cursor = 0;
            }
            KeyCode::Backspace => {
                filter.input.backspace();
                app.main_cursor = 0;
            }
            KeyCode::Char(c) => {
                filter.input.insert(c);
                app.main_cursor = 0;
            }
            _ => {}
        }
        return;
    }

    let items = app.main_items();
    let last = items.len().saturating_sub(1);

    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                reorder_main_cursor(app, -1);
            } else {
                app.main_cursor = if app.main_cursor == 0 {
                    last
                } else {
                    app.main_cursor - 1
                };
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                reorder_main_cursor(app, 1);
            } else {
                app.main_cursor = if app.main_cursor >= last {
                    0
                } else {
                    app.main_cursor + 1
                };
            }
        }
        KeyCode::Enter | KeyCode::Char('m') => activate_main_item(app),
        KeyCode::Char('q') | KeyCode::Esc => {
            if app.filter.is_some() {
                app.filter = None;
                app.main_cursor = 0;
            } else {
                app.quit = true;
            }
        }
        KeyCode::Char('?') => app.modals.push(Modal::Help),
        KeyCode::Char('r') => {
            app.manual_refresh();
            app.toast(ToastKind::Info, "refreshing usage…");
        }
        KeyCode::Char('/') => {
            app.filter = Some(FilterState {
                input: InputState::new(""),
                focused: true,
            });
            app.main_cursor = 0;
        }
        _ => {}
    }
}

fn activate_main_item(app: &mut App) {
    let Some(item) = app.current_main_item() else {
        return;
    };
    match item {
        MainItemKind::Profile(idx) => {
            let Some(name) = app.config.profiles.get(idx).map(|p| p.name.clone()) else {
                return;
            };
            app.modals
                .push(Modal::ProfileMenu(ProfileMenuState { name, cursor: 0 }));
        }
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
        1 if idx + 1 < app.config.profiles.len() => idx + 1,
        _ => return,
    };
    if let Err(e) = reorder_profile(&mut app.config, idx, new_idx) {
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
    let _ = oauth::refresh_all(&mut app.config);
    match switch_profile(&mut app.config, name) {
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
    let Some(profile) = app.config.find(name) else {
        return;
    };
    app.modals.push(Modal::EditProfile(EditProfileForm {
        name: name.to_string(),
        base_url: InputState::new(profile.base_url.as_deref().unwrap_or("")),
        api_key: InputState::new(profile.api_key.as_deref().unwrap_or("")),
        focus: EndpointField::BaseUrl,
    }));
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
    if let Some(existing) = find_matching_oauth_profile(&app.config, snapshot.credentials.as_ref())
    {
        let existing = existing.to_string();
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
    let mut items: Vec<ChainItemKind> = app
        .config
        .state
        .fallback_chain
        .iter()
        .enumerate()
        .map(|(i, _)| ChainItemKind::Member(i))
        .collect();
    let any_unchained = app
        .config
        .profiles
        .iter()
        .any(|p| !app.config.state.fallback_chain.iter().any(|c| c == &p.name));
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
            let Some(name) = app.config.state.fallback_chain.get(i).cloned() else {
                return;
            };
            app.modals
                .push(Modal::ChainItemMenu(ChainItemMenuState { name, cursor: 0 }));
        }
        ChainItemKind::Add => {
            let candidates: Vec<String> = app
                .config
                .profiles
                .iter()
                .filter(|p| !app.config.state.fallback_chain.iter().any(|c| c == &p.name))
                .map(|p| p.name.clone())
                .collect();
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
        Modal::ReconcileKeep { .. } => handle_reconcile_keep_key(app, key),
        Modal::ReconcileCaptureAsk { .. } => handle_reconcile_capture_key(app, key),
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
        .config
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
        // Single configuration entry point — Enter and `m` both open the
        // per-profile menu. Every per-profile setting lives there.
        KeyCode::Enter | KeyCode::Char('m') => {
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
    let Some(profile) = app.config.find_mut(name) else {
        return;
    };
    if !profile.is_oauth() {
        app.toast(
            ToastKind::Warning,
            "auto-start usage only applies to OAuth profiles",
        );
        return;
    }
    profile.auto_start = !profile.auto_start;
    let now_on = profile.auto_start;
    match save_profile(profile) {
        Ok(()) => {
            let body = if now_on {
                format!("auto-start usage on for '{name}'")
            } else {
                format!("auto-start usage off for '{name}'")
            };
            app.toast(ToastKind::Success, body);
        }
        Err(e) => {
            // Roll back so the disk + memory stay aligned.
            if let Some(p) = app.config.find_mut(name) {
                p.auto_start = !now_on;
            }
            app.toast(ToastKind::Danger, format!("save failed: {e}"));
        }
    }
}

/// Options shown in the per-profile actions menu. Chain composition lives
/// in the chain screen — not here — so this menu stays focused on
/// profile-level concerns.
pub(crate) fn profile_menu_options(app: &App, name: &str) -> Vec<ProfileMenuAction> {
    let mut out = Vec::with_capacity(7);
    let profile = app.config.find(name);
    let is_active = app.config.is_active(name);
    let is_oauth = profile.map(|p| p.is_oauth()).unwrap_or(false);

    if !is_active {
        out.push(ProfileMenuAction::Switch);
    }
    out.push(ProfileMenuAction::Details);
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
        ProfileMenuAction::Switch => {
            app.modals.pop();
            perform_switch(app, name);
        }
        ProfileMenuAction::Details => {
            let idx = app.config.profiles.iter().position(|p| p.name == name);
            app.modals.pop();
            if let Some(idx) = idx {
                app.screen = Screen::ProfileDetail { profile_index: idx };
            }
        }
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
    let existing = app.config.names();
    if let Err(e) = validate_profile_name(&name, &existing, None) {
        app.toast(ToastKind::Danger, format!("{e}"));
        return;
    }
    // API key only makes sense for endpoint profiles. Drop it if no URL.
    let api_key = if base_url.is_some() { api_key } else { None };
    match create_blank_profile(&mut app.config, name.clone(), base_url, api_key) {
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
    match edit_profile_endpoint(&mut app.config, &name, base_url, api_key) {
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
            let existing = app.config.names();
            if let Err(e) = validate_profile_name(&new, &existing, Some(old.as_str())) {
                app.toast(ToastKind::Danger, format!("{e}"));
                return;
            }
            match rename_profile(&mut app.config, &old, &new) {
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
        ConfirmAction::Delete(name) => match delete_profile(&mut app.config, &name) {
            Ok(()) => {
                app.refresh_tokens();
                app.last_state_mtime = app_state_mtime();
                app.clamp_main_cursor();
                app.toast(ToastKind::Success, format!("deleted '{name}'"));
            }
            Err(e) => app.toast(ToastKind::Danger, format!("delete failed: {e}")),
        },
        ConfirmAction::CaptureConflict(snapshot) => {
            app.modals.push(Modal::CaptureName(CaptureNameForm {
                snapshot,
                input: InputState::new(""),
            }));
        }
    }
}

fn handle_reconcile_keep_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::ReconcileKeep { choice, .. }) = app.modals.last_mut() else {
        return;
    };
    match key.code {
        KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::Char('h' | 'l') => {
            *choice = !*choice;
        }
        KeyCode::Char('y') => *choice = true,
        KeyCode::Char('n') => *choice = false,
        KeyCode::Esc => {
            // Esc = "keep" — the safer fallback.
            let _ = snapshot_active_credentials(&mut app.config);
            app.modals.pop();
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let keep = *choice;
            app.modals.pop();
            if keep {
                let _ = snapshot_active_credentials(&mut app.config);
            } else {
                let _ = detach_credentials_link();
                app.config.state.active_profile = None;
                let _ = save_app_state(&app.config.state);
                app.modals.push(Modal::ReconcileCaptureAsk { choice: true });
            }
        }
        _ => {}
    }
}

fn handle_reconcile_capture_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::ReconcileCaptureAsk { choice }) = app.modals.last_mut() else {
        return;
    };
    match key.code {
        KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::Char('h' | 'l') => {
            *choice = !*choice;
        }
        KeyCode::Char('y') => *choice = true,
        KeyCode::Char('n') => *choice = false,
        KeyCode::Esc => {
            app.modals.pop();
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let go = *choice;
            app.modals.pop();
            if go {
                begin_capture(app);
            }
        }
        _ => {}
    }
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
            let existing = app.config.names();
            if let Err(e) = validate_profile_name(&name, &existing, None) {
                app.toast(ToastKind::Danger, format!("{e}"));
                return;
            }
            // Consume the boxed snapshot out of the modal.
            let Some(Modal::CaptureName(form)) = app.modals.pop() else {
                return;
            };
            let snapshot = *form.snapshot;
            match capture_into_profile(&mut app.config, name.clone(), snapshot) {
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
    let chain_len = app.config.state.fallback_chain.len();
    let position = app
        .config
        .state
        .fallback_chain
        .iter()
        .position(|n| n == &state.name);
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
                        app.config.state.fallback_chain.swap(p - 1, p);
                        let _ = save_app_state(&app.config.state);
                        if app.chain_cursor > 0 {
                            app.chain_cursor -= 1;
                        }
                    }
                }
                ChainAction::MoveDown => {
                    if let Some(p) = position
                        && p + 1 < chain_len
                    {
                        app.config.state.fallback_chain.swap(p, p + 1);
                        let _ = save_app_state(&app.config.state);
                        if app.chain_cursor + 1 < chain_items(app).len() {
                            app.chain_cursor += 1;
                        }
                    }
                }
                ChainAction::Remove => {
                    app.config.state.fallback_chain.retain(|n| n != &name);
                    let _ = save_app_state(&app.config.state);
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
            if let Some(profile) = app.config.find_mut(&name)
                && profile.fallback_threshold.is_none()
            {
                profile.fallback_threshold = Some(DEFAULT_THRESHOLD);
                let _ = save_profile(profile);
            }
            app.config.state.fallback_chain.push(name);
            let _ = save_app_state(&app.config.state);
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
            if let Some(profile) = app.config.find_mut(&name) {
                profile.fallback_threshold = Some(value);
                if let Err(e) = save_profile(profile) {
                    app.toast(ToastKind::Danger, format!("save failed: {e}"));
                    return;
                }
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

    // Refresher just finished a cycle. Now is the moment to check whether any
    // opted-in profile is still missing a 5h window and prime one. Doing this
    // here (instead of inside the refresher thread) keeps the mutation path
    // on the main thread, reusing existing &mut AppConfig flows. The kick
    // function short-circuits in microseconds when nothing needs work, so
    // the per-tick cost is only paid when there's actual work to do.
    let busy = app.activity.load(Ordering::Relaxed);
    if app.was_busy && !busy {
        if app.just_auto_started {
            // Idle edge from the manual_refresh we kicked off below; skip one
            // cycle so we don't re-enter auto-start while waiting for usage to
            // report the freshly-opened 5h window.
            app.just_auto_started = false;
        } else {
            let started = oauth::auto_start_windows(&mut app.config, &app.usage_store);
            if !started.is_empty() {
                app.refresh_tokens();
                app.manual_refresh();
                app.just_auto_started = true;
                let body = if started.len() == 1 {
                    format!("auto-started usage window for '{}'", started[0])
                } else {
                    format!("auto-started {} usage windows", started.len())
                };
                app.toast(ToastKind::Info, body);
            }
        }
    }
    app.was_busy = busy;

    if let Ok(Some(target)) = auto_switch_if_needed(&mut app.config) {
        app.refresh_tokens();
        app.last_state_mtime = app_state_mtime();
        app.toast(ToastKind::Warning, format!("auto-switched to '{target}'"));
    }
    app.prune_toasts();
}

// ── Shutdown ──────────────────────────────────────────────────────────────────

/// Persist whatever Claude Code wrote during this session, then replace the
/// symlink with a plain copy. After shutdown any external write to
/// ~/.claude/.credentials.json lands in that standalone file instead of
/// mutating the active profile's storage through the link.
pub(crate) fn shutdown(app: &mut App) -> Result<()> {
    let _ = snapshot_active_credentials(&mut app.config);
    let _ = detach_credentials_link();
    Ok(())
}
