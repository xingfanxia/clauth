//! Footer hint-bar pins.

use super::*;
use crate::profile::{AppConfig, AppState};

fn bare_app() -> App {
    App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    })
}

/// The Usage tab advertises the note editor key while an account exists, and
/// drops it on an empty roster — there `n` still starts a new account (the
/// hint derives from the key's behavior on this frame).
#[test]
fn the_usage_hints_advertise_the_note_key() {
    use crate::profile::Profile;
    use std::collections::BTreeMap;
    let _home = crate::testutil::HomeSandbox::new();
    let profile = Profile {
        name: "alice".into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        preferred: false,
        preferred_days: Vec::new(),
        rolling_token: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        console: None,
        credentials: None,
        provider: None,
    };
    let with_account = App::new(crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![profile],
    });
    let mut app = with_account;
    app.tab = Tab::Usage;
    let hints = tab_hints(&app);
    assert!(
        hints.contains(&("n", "note")),
        "usage hints must name the note key, got {hints:?}"
    );

    let mut empty = bare_app();
    empty.tab = Tab::Usage;
    let hints = tab_hints(&empty);
    assert!(
        hints.iter().all(|(k, _)| *k != "n"),
        "an empty roster keeps n = new account off the hint bar, got {hints:?}"
    );
}

/// On the Setup tab's `alert at` row the bar names the stepper keys, the same
/// pair the Fallback card's threshold row names.
#[test]
fn the_setup_alert_row_advertises_its_stepper_keys() {
    use crate::tui::app::{ConfigFocus, ConfigRow, config_rows};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![crate::profile::Profile::new("a".to_string(), None, None)],
    });
    app.tab = Tab::Setup;
    app.config_focus = ConfigFocus::Actions;
    app.profile_cursor = 0;
    app.config_action_cursor = config_rows(&app)
        .iter()
        .position(|r| *r == ConfigRow::BellThreshold)
        .expect("the alert row is listed");
    assert_eq!(
        tab_hints(&app),
        [
            ("↑↓", "row"),
            ("+", "raise"),
            ("-", "lower"),
            ("↵", "type"),
            ("a", "actions"),
            ("?", "help"),
            ("q", "back"),
        ]
    );
}

/// Ruling 10: a draft field's typed editor reads `↵ done   ←→ caret   esc
/// revert`, a draft cycle row's pending pick `↵ done   esc revert` — the Setup
/// `+ new` form's `done` word, `esc revert` kept because the draft editor's
/// `esc` restores. B's P2-B1 plant (routing both to the non-draft hints) reds
/// this.
#[test]
fn the_draft_editors_read_done_not_save() {
    use crate::tui::app::{
        InputState, ShuntAddKind, ShuntDraftField, ShuntFieldEdit, ShuntFieldEditState,
        ShuntFieldKey, UpstreamFieldKind,
    };
    let _home = crate::testutil::HomeSandbox::new();
    let draft_key = ShuntFieldKey::Draft {
        kind: ShuntAddKind::Upstream,
        field: ShuntDraftField::Upstream(UpstreamFieldKind::Name),
    };

    let mut typed = bare_app();
    typed.services.shunt_field_edit = Some(ShuntFieldEditState {
        key: draft_key.clone(),
        edit: ShuntFieldEdit::Typed {
            input: InputState::new(""),
            saved: None,
        },
        refusal: None,
    });
    assert_eq!(
        owner_hints(KeyOwner::ShuntField, &typed),
        Some(&[("↵", "done"), ("←→", "caret"), ("esc", "revert")][..]),
        "a draft field's typed editor reads `↵ done`, never `↵ save`"
    );

    let mut pick = bare_app();
    pick.services.shunt_field_edit = Some(ShuntFieldEditState {
        key: draft_key,
        edit: ShuntFieldEdit::Pick {
            saved: None,
            picked: "api_key".into(),
        },
        refusal: None,
    });
    assert_eq!(
        owner_hints(KeyOwner::ShuntField, &pick),
        Some(&[("↵", "done"), ("esc", "revert")][..]),
        "a draft cycle row's pending pick reads `↵ done   esc revert`"
    );
}

/// The re-bind confirm's footer row is pinned: the ruled `space toggle`,
/// `↵ choose`, `q not now` copy plus the derived `? help` before `q`.
#[test]
fn the_rebind_confirm_footer_row_is_pinned() {
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();
    let profiles = vec![
        Profile::new(
            "D1".to_string(),
            Some("https://api.deepseek.com/anthropic".to_string()),
            None,
        ),
        Profile::new(
            "D2".to_string(),
            Some("https://api.deepseek.com/anthropic".to_string()),
            None,
        ),
    ];
    let app = App::new(AppConfig {
        state: AppState {
            profiles: profiles.iter().map(|p| p.name.clone()).collect(),
            ..AppState::default()
        },
        profiles,
    });
    assert!(
        matches!(app.modals.first(), Some(Modal::Rebind(_))),
        "the re-bind confirm is open"
    );

    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 40)).expect("backend");
    term.draw(|f| crate::tui::render::draw(f, &app))
        .expect("draw");
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let footer = rows.last().expect("the footer row").trim();
    assert_eq!(
        footer, "space toggle   ↵ choose   ? help   q not now",
        "the footer row is the ruled copy with the derived ? help",
    );
}
