use super::*;
use crate::profile::{ClaudeCredentials, OAuthToken};
use std::fs;

fn creds(access: &str, refresh: Option<&str>) -> ClaudeCredentials {
    ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: access.to_string(),
            refresh_token: refresh.map(str::to_string),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    }
}

#[test]
fn diverged_returns_false_when_either_side_missing() {
    let c = creds("a", Some("r"));
    assert!(!credentials_diverged(None, Some(&c)));
    assert!(!credentials_diverged(Some(&c), None));
    assert!(!credentials_diverged(None, None));
}

#[test]
fn diverged_returns_false_when_tokens_match() {
    let a = creds("access-1", Some("refresh-1"));
    let b = creds("access-1", Some("refresh-1"));
    assert!(!credentials_diverged(Some(&a), Some(&b)));
}

#[test]
fn diverged_returns_true_when_access_token_differs() {
    let a = creds("access-1", Some("refresh-1"));
    let b = creds("access-2", Some("refresh-1"));
    assert!(credentials_diverged(Some(&a), Some(&b)));
}

#[test]
fn diverged_returns_true_when_refresh_token_differs() {
    let a = creds("access-1", Some("refresh-1"));
    let b = creds("access-1", Some("refresh-2"));
    assert!(credentials_diverged(Some(&a), Some(&b)));
}

#[test]
fn diverged_returns_true_when_refresh_token_disappears() {
    let a = creds("access-1", Some("refresh-1"));
    let b = creds("access-1", None);
    assert!(credentials_diverged(Some(&a), Some(&b)));
}

#[test]
fn diverged_returns_false_when_oauth_block_missing_on_one_side() {
    let with = creds("a", Some("r"));
    let without = ClaudeCredentials {
        claude_ai_oauth: None,
    };
    assert!(!credentials_diverged(Some(&with), Some(&without)));
    assert!(!credentials_diverged(Some(&without), Some(&with)));
}

#[test]
fn classify_link_missing_when_path_absent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::Missing,
    );
}

#[test]
fn classify_link_diverged_when_plain_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    fs::write(&link, b"{}").expect("write live");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::Diverged,
    );
}

#[cfg(unix)]
#[test]
fn classify_link_linked_to_when_pointing_at_expected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    fs::write(&expected, b"{}").expect("write target");
    std::os::unix::fs::symlink(&expected, &link).expect("symlink");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::LinkedTo,
    );
}

#[cfg(unix)]
#[test]
fn classify_link_diverged_when_symlink_points_elsewhere() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    let other = tmp.path().join("other.json");
    fs::write(&other, b"{}").expect("write other");
    fs::write(&expected, b"{}").expect("write target");
    std::os::unix::fs::symlink(&other, &link).expect("symlink");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::Diverged,
    );
}

#[test]
fn first_login_true_when_no_stored_creds_and_plain_oauth_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    fs::write(
        &link,
        serde_json::to_vec(&creds("a", Some("r"))).expect("ser"),
    )
    .expect("write");
    assert!(is_first_login_at(&link, &expected));
}

#[test]
fn first_login_false_when_stored_creds_exist() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    fs::write(
        &link,
        serde_json::to_vec(&creds("a", Some("r"))).expect("ser"),
    )
    .expect("write");
    fs::write(&expected, b"{}").expect("write stored");
    assert!(!is_first_login_at(&link, &expected));
}

#[test]
fn first_login_false_when_link_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    assert!(!is_first_login_at(&link, &expected));
}

#[test]
fn first_login_false_when_oauth_block_absent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    // valid JSON but no OAuth block — mid-flight partial write
    fs::write(&link, b"{}").expect("write");
    assert!(!is_first_login_at(&link, &expected));
}

#[cfg(unix)]
#[test]
fn first_login_false_when_link_is_symlink() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    let store = tmp.path().join("store.json");
    fs::write(
        &store,
        serde_json::to_vec(&creds("a", Some("r"))).expect("ser"),
    )
    .expect("write");
    std::os::unix::fs::symlink(&store, &link).expect("symlink");
    assert!(!is_first_login_at(&link, &expected));
}

#[cfg(unix)]
#[test]
fn classify_link_linked_to_even_when_target_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    std::os::unix::fs::symlink(&expected, &link).expect("symlink");
    // target absent (e.g. first-ever link, before save_profile writes it)
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::LinkedTo,
    );
}
