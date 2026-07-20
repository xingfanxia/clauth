//! `clauth sessions/resume/info` CLI surface tests. Fixture stores live under a
//! `HomeSandbox` so the global (`~/.claude/projects`) walk stays off the real
//! tree. Each transcript is named `<sessionId>.jsonl` (the id is the filename
//! stem). Pure helpers (`resume_profile_choice`, `sessions_json`) are exercised
//! directly; the exit-code contract goes through `crate::exit_code`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::collections::HashMap;
use std::fs;
use std::time::{Duration, SystemTime};

use serde_json::json;

use crate::testutil::{HomeSandbox, set_mtime};

fn write_jsonl(path: &Path, lines: &[String]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, lines.join("\n")).unwrap();
}

fn user_line(sid: &str, cwd: &str, text: &str) -> String {
    json!({"sessionId": sid, "cwd": cwd, "message": {"role": "user", "content": text}}).to_string()
}

/// An assistant usage line — the token-bearing row `file_model_tokens` reads.
fn usage_line(sid: &str, cwd: &str, msg_id: &str, model: &str, input: u64, output: u64) -> String {
    json!({
        "sessionId": sid, "cwd": cwd, "timestamp": "2026-06-11T10:30:00+00:00",
        "message": {
            "id": msg_id, "role": "assistant", "model": model,
            "usage": {
                "input_tokens": input, "output_tokens": output,
                "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0
            }
        }
    })
    .to_string()
}

/// A `PriceTable` from `(model_id, input_rate, output_rate)` rows; cache rates 0.
fn price_table(rows: &[(&str, f64, f64)]) -> crate::pricing::PriceTable {
    let mut rates = HashMap::new();
    for &(id, input, output) in rows {
        rates.insert(
            id.to_owned(),
            crate::pricing::ModelRate {
                input,
                output,
                cache_read: 0.0,
                cache_write: 0.0,
            },
        );
    }
    crate::pricing::PriceTable::from_rates(rates)
}

// ── clauth sessions --json ──

#[test]
fn sessions_json_has_exact_fields_newest_first_with_null_and_redaction() {
    let sb = HomeSandbox::new();

    // Newer session: a token-bearing usage row + a secret in the first message.
    let a = sb.home().join(".claude/projects/-w-a/aaaa-1111.jsonl");
    write_jsonl(
        &a,
        &[
            user_line(
                "aaaa-1111",
                "/ws/a",
                "my key sk-ant-api03-ABCDEFGHIJKLMNOPQRST here",
            ),
            usage_line("aaaa-1111", "/ws/a", "m1", "claude-sonnet-4", 100, 50),
        ],
    );

    // Older session: no usage row, so its token total stays absent (JSON null).
    let b = sb.home().join(".claude/projects/-w-b/bbbb-2222.jsonl");
    write_jsonl(&b, &[user_line("bbbb-2222", "/ws/b", "plain question")]);
    set_mtime(&b, SystemTime::now() - Duration::from_secs(3600));

    let mut groups = crate::sessions::build_index();
    let price = price_table(&[("claude-sonnet-4", 0.000003, 0.000015)]);
    crate::sessions::annotate_all(&mut groups, Some(&price));
    crate::sessions::annotate_owners(&mut groups);
    let flat = flatten_newest_first(&groups);
    let value = sessions_json(&flat);

    let arr = value.as_array().expect("json array");
    assert_eq!(arr.len(), 2, "both sessions present");

    // Newest-first: the token-bearing session (fresh mtime) leads.
    assert_eq!(arr[0]["id"], json!("aaaa-1111"), "newest session first");
    assert_eq!(arr[1]["id"], json!("bbbb-2222"));

    // Exactly the documented field set — no more, no less.
    let keys: std::collections::BTreeSet<&str> = arr[0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let want: std::collections::BTreeSet<&str> = [
        "id",
        "last_ran_profile",
        "workspace",
        "updated",
        "first_message",
        "last_message",
        "tokens",
        "cost",
    ]
    .into_iter()
    .collect();
    assert_eq!(
        keys, want,
        "json row must carry exactly the documented fields"
    );

    // Tokenless session ⇒ JSON null, never 0.
    assert_eq!(arr[1]["tokens"], json!(null), "absent tokens must be null");
    assert_eq!(arr[1]["cost"], json!(null), "absent cost must be null");

    // Priced token-bearing session ⇒ a real number, not null.
    assert!(
        arr[0]["tokens"].is_number(),
        "priced session has a token total"
    );
    assert!(arr[0]["cost"].is_number(), "priced session has a cost");

    // `updated` is an ISO-8601 UTC string.
    let updated = arr[0]["updated"].as_str().expect("updated is a string");
    assert!(
        updated.contains('T') && updated.ends_with("+00:00"),
        "updated must be ISO-8601 UTC, got {updated}"
    );

    // Redaction survives into the emitted preview.
    let preview = arr[0]["first_message"]
        .as_str()
        .expect("first_message string");
    assert!(
        preview.contains("[REDACTED]") && !preview.contains("sk-ant-api03-ABCDEFGHIJKLMNOPQRST"),
        "the secret must be redacted in the preview, got {preview}"
    );
}

// ── exit-code contract (0 / 1 / 2) ──

#[test]
fn no_sessions_found_maps_to_exit_one() {
    let _sb = HomeSandbox::new(); // empty tree ⇒ empty index
    let err = run_sessions(true).expect_err("empty index must error");
    assert!(
        err.to_string().contains("no sessions"),
        "error must say no sessions were found: {err}"
    );
    assert!(
        err.downcast_ref::<crate::UsageError>().is_none(),
        "an empty index is a runtime error, not a usage error"
    );
    assert_eq!(crate::exit_code(Err(err)), 1);
}

#[test]
fn sessions_bad_flag_maps_to_exit_two() {
    // Through the real dispatch arm: an unknown `sessions` flag is a usage error.
    let args = ["sessions".to_string(), "--bogus".to_string()];
    let err = crate::dispatch(&args).expect_err("bad flag must error");
    assert!(
        err.downcast_ref::<crate::UsageError>().is_some(),
        "a bad flag must be a UsageError"
    );
    assert_eq!(crate::exit_code(Err(err)), 2);
}

#[test]
fn success_maps_to_exit_zero() {
    assert_eq!(crate::exit_code(Ok(())), 0);
}

// ── resume_profile_choice: the four branches ──

#[test]
fn resume_profile_choice_explicit_flag_forces_no_prompt() {
    // A flag wins regardless of tty or a known last-ran profile.
    assert_eq!(
        resume_profile_choice(Some("chosen"), true, Some("lastran"), "active"),
        ("chosen".to_string(), false)
    );
    assert_eq!(
        resume_profile_choice(Some("chosen"), false, None, "active"),
        ("chosen".to_string(), false)
    );
}

#[test]
fn resume_profile_choice_piped_no_flag_uses_active_forced() {
    assert_eq!(
        resume_profile_choice(None, false, Some("lastran"), "active"),
        ("active".to_string(), false)
    );
}

#[test]
fn resume_profile_choice_tty_known_last_ran_prompts_defaulting_to_it() {
    // Mutation target: if this branch returned `active`, this test fails.
    assert_eq!(
        resume_profile_choice(None, true, Some("lastran"), "active"),
        ("lastran".to_string(), true)
    );
}

#[test]
fn resume_profile_choice_tty_unknown_prompts_defaulting_to_active() {
    assert_eq!(
        resume_profile_choice(None, true, None, "active"),
        ("active".to_string(), true)
    );
}

// ── resume <unknown id> ──

#[test]
fn resume_unknown_id_errors_naming_it_at_exit_one() {
    let sb = HomeSandbox::new();
    // A real session so the index isn't empty — the error must be "unknown id",
    // not "no sessions".
    let path = sb.home().join(".claude/projects/-w/known-session.jsonl");
    write_jsonl(&path, &[user_line("known-session", "/ws", "hi")]);

    let err = run_resume("ghost-id", None).expect_err("unknown id must error");
    assert!(
        err.to_string().contains("ghost-id"),
        "the error must name the unknown id: {err}"
    );
    assert!(
        err.downcast_ref::<crate::UsageError>().is_none(),
        "an unknown id is a runtime error, not a usage error"
    );
    assert_eq!(crate::exit_code(Err(err)), 1);
}
