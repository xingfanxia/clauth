use super::*;

use std::io::Write as _;
use std::time::{Duration, SystemTime};

use crate::testutil::{HomeSandbox, set_mtime};

// ── helpers ──────────────────────────────────────────────────────────────────

fn write_stats_cache(claude_dir: &std::path::Path, json: &str) {
    std::fs::write(claude_dir.join("stats-cache.json"), json).expect("write stats-cache");
}

fn make_claude_dir(sandbox: &HomeSandbox) -> std::path::PathBuf {
    let dir = sandbox.home().join(".claude");
    std::fs::create_dir_all(&dir).expect("create .claude");
    dir
}

// ── 1. base stats parsing ─────────────────────────────────────────────────────

#[test]
fn base_stats_parsed_correctly() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);

    write_stats_cache(
        &claude_dir,
        r#"{
            "version": 1,
            "lastComputedDate": "2026-06-10",
            "firstSessionDate": "2025-01-01T00:00:00+00:00",
            "totalSessions": 42,
            "totalMessages": 1000,
            "dailyActivity": [
                {"date": "2026-06-09", "messageCount": 50, "sessionCount": 3, "toolCallCount": 120},
                {"date": "2026-06-10", "messageCount": 70, "sessionCount": 4, "toolCallCount": 200}
            ],
            "dailyModelTokens": [
                {"date": "2026-06-09", "tokensByModel": {"claude-opus-4": 5000, "gpt-5": 1000}},
                {"date": "2026-06-10", "tokensByModel": {"claude-opus-4": 8000}}
            ],
            "modelUsage": {
                "claude-opus-4": {
                    "inputTokens": 10000,
                    "outputTokens": 5000,
                    "cacheReadInputTokens": 2000,
                    "cacheCreationInputTokens": 500
                },
                "gpt-5": {
                    "inputTokens": 3000,
                    "outputTokens": 1000,
                    "cacheReadInputTokens": 0,
                    "cacheCreationInputTokens": 0
                }
            },
            "hourCounts": {"0": 10, "12": 200, "23": 50}
        }"#,
    );

    let stats = load(&claude_dir).expect("load must succeed");

    // models sorted DESC by total
    assert_eq!(stats.models.len(), 2);
    assert_eq!(stats.models[0].model, "claude-opus-4");
    assert_eq!(stats.models[0].input, 10000);
    assert_eq!(stats.models[0].output, 5000);
    assert_eq!(stats.models[0].cache_read, 2000);
    assert_eq!(stats.models[0].cache_create, 500);
    assert_eq!(stats.models[1].model, "gpt-5");

    // daily summed across models, sorted ASC
    assert_eq!(stats.daily.len(), 2);
    assert_eq!(stats.daily[0].date, "2026-06-09");
    assert_eq!(stats.daily[0].tokens, 6000); // 5000 + 1000
    assert_eq!(stats.daily[1].date, "2026-06-10");
    assert_eq!(stats.daily[1].tokens, 8000);

    // activity sorted ASC
    assert_eq!(stats.activity.len(), 2);
    assert_eq!(stats.activity[0].date, "2026-06-09");
    assert_eq!(stats.activity[0].messages, 50);
    assert_eq!(stats.activity[0].sessions, 3);
    assert_eq!(stats.activity[0].tool_calls, 120);

    // hour_counts: present keys mapped, absent keys = 0
    assert_eq!(stats.hour_counts[0], 10);
    assert_eq!(stats.hour_counts[12], 200);
    assert_eq!(stats.hour_counts[23], 50);
    assert_eq!(stats.hour_counts[1], 0);
    assert_eq!(stats.hour_counts[11], 0);

    // totals
    assert_eq!(stats.total_input, 13000);
    assert_eq!(stats.total_output, 6000);
    assert_eq!(stats.total_cache_read, 2000);
    assert_eq!(stats.total_cache_create, 500);
    assert_eq!(stats.total_sessions, 42);
    assert_eq!(stats.total_messages, 1000);
    assert_eq!(
        stats.first_session_date.as_deref(),
        Some("2025-01-01T00:00:00+00:00")
    );
    assert_eq!(stats.last_computed_date.as_deref(), Some("2026-06-10"));
    assert!(stats.topped_up_through.is_none());
}

#[test]
fn load_returns_none_when_cache_absent() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    // no stats-cache.json written
    assert!(load(&claude_dir).is_none());
}

// ── 2. group_models ───────────────────────────────────────────────────────────

#[test]
fn group_models_keeps_claude_individual_folds_others() {
    let models = vec![
        ModelTokens {
            model: "claude-opus-4-8".to_owned(),
            input: 1000,
            output: 500,
            cache_read: 100,
            cache_create: 50,
        },
        ModelTokens {
            model: "gpt-5.5".to_owned(),
            input: 200,
            output: 100,
            cache_read: 0,
            cache_create: 0,
        },
        ModelTokens {
            model: "gemini-3-flash".to_owned(),
            input: 300,
            output: 150,
            cache_read: 0,
            cache_create: 0,
        },
        ModelTokens {
            model: "claude-sonnet-4".to_owned(),
            input: 500,
            output: 250,
            cache_read: 50,
            cache_create: 25,
        },
    ];

    let grouped = group_models(&models);

    // claude models individual, others folded
    let claude_rows: Vec<_> = grouped
        .iter()
        .filter(|m| m.model.starts_with("claude"))
        .collect();
    let others_rows: Vec<_> = grouped.iter().filter(|m| m.model == "others").collect();
    assert_eq!(claude_rows.len(), 2);
    assert_eq!(others_rows.len(), 1);

    let others = &others_rows[0];
    assert_eq!(others.input, 500); // 200 + 300
    assert_eq!(others.output, 250); // 100 + 150

    // sorted DESC by in+out (the dashboard basis)
    let in_outs: Vec<u64> = grouped.iter().map(|m| m.in_out()).collect();
    for pair in in_outs.windows(2) {
        assert!(pair[0] >= pair[1], "not sorted desc: {pair:?}");
    }
}

#[test]
fn group_models_no_others_when_all_claude() {
    let models = vec![ModelTokens {
        model: "claude-opus-4".to_owned(),
        input: 100,
        output: 50,
        cache_read: 0,
        cache_create: 0,
    }];
    let grouped = group_models(&models);
    assert!(grouped.iter().all(|m| m.model != "others"));
}

#[test]
fn group_models_empty_input() {
    assert!(group_models(&[]).is_empty());
}

#[test]
fn group_models_breaks_out_large_non_anthropic() {
    let models = vec![
        // > 1M total → shown individually even though non-Anthropic.
        ModelTokens {
            model: "gpt-5.5".to_owned(),
            input: 2_000_000,
            output: 100_000,
            cache_read: 0,
            cache_create: 0,
        },
        // < 1M total → folds into "others".
        ModelTokens {
            model: "tiny-model".to_owned(),
            input: 100,
            output: 50,
            cache_read: 0,
            cache_create: 0,
        },
        ModelTokens {
            model: "claude-opus-4-8".to_owned(),
            input: 500,
            output: 250,
            cache_read: 0,
            cache_create: 0,
        },
    ];
    let grouped = group_models(&models);
    assert!(
        grouped.iter().any(|m| m.model == "gpt-5.5"),
        "a >1M non-Anthropic model must show separately"
    );
    assert!(grouped.iter().any(|m| m.model == "claude-opus-4-8"));
    let others = grouped
        .iter()
        .find(|m| m.model == "others")
        .expect("the tiny model must fold into others");
    assert_eq!(others.in_out(), 150); // only tiny-model (100 + 50)
}

// ── 3. is_anthropic ───────────────────────────────────────────────────────────

#[test]
fn is_anthropic_recognition() {
    assert!(is_anthropic("claude-opus-4-8"));
    assert!(is_anthropic("claude-fable-5"));
    assert!(is_anthropic("claude-sonnet-4-20260101"));
    assert!(!is_anthropic("gpt-5.5"));
    assert!(!is_anthropic("gemini-3-flash"));
    assert!(!is_anthropic("deepseek-r2"));
    assert!(!is_anthropic(""));
}

#[test]
fn model_display_name_mapping() {
    assert_eq!(model_display_name("claude-opus-4-8"), "opus 4.8");
    assert_eq!(model_display_name("claude-sonnet-4-6"), "sonnet 4.6");
    assert_eq!(model_display_name("claude-haiku-4-5-20251001"), "haiku 4.5");
    assert_eq!(
        model_display_name("claude-sonnet-4-5-20250929"),
        "sonnet 4.5"
    );
    assert_eq!(
        model_display_name("claude-opus-4-6-thinking"),
        "opus 4.6 thinking"
    );
    assert_eq!(model_display_name("claude-sonnet-4.6"), "sonnet 4.6");
    assert_eq!(model_display_name("claude-fable-5"), "fable 5");
    // Non-Anthropic and the synthetic bucket pass through.
    assert_eq!(model_display_name("gpt-5.5"), "gpt-5.5");
    assert_eq!(model_display_name("others"), "others");
}

// ── 4. cache_hit_ratio ────────────────────────────────────────────────────────

#[test]
fn cache_hit_ratio_math() {
    let stats = TokenStats {
        models: vec![],
        daily: vec![],
        activity: vec![],
        hour_counts: [0; 24],
        total_input: 1000,
        total_output: 0,
        total_cache_read: 500,
        total_cache_create: 500,
        total_sessions: 0,
        total_messages: 0,
        first_session_date: None,
        last_computed_date: None,
        topped_up_through: None,
        today: None,
    };
    // cache_read / (cache_read + cache_create + input) = 500 / 2000 = 0.25
    let ratio = stats.cache_hit_ratio();
    assert!((ratio - 0.25).abs() < 1e-9, "expected 0.25 got {ratio}");
}

#[test]
fn cache_hit_ratio_zero_denominator() {
    let stats = TokenStats {
        models: vec![],
        daily: vec![],
        activity: vec![],
        hour_counts: [0; 24],
        total_input: 0,
        total_output: 0,
        total_cache_read: 0,
        total_cache_create: 0,
        total_sessions: 0,
        total_messages: 0,
        first_session_date: None,
        last_computed_date: None,
        topped_up_through: None,
        today: None,
    };
    assert_eq!(stats.cache_hit_ratio(), 0.0);
}

// ── 5. top-up ────────────────────────────────────────────────────────────────

fn jsonl_line(
    timestamp: &str,
    model: &str,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_create: u64,
) -> String {
    format!(
        r#"{{"timestamp":"{timestamp}","message":{{"model":"{model}","usage":{{"input_tokens":{input},"output_tokens":{output},"cache_read_input_tokens":{cache_read},"cache_creation_input_tokens":{cache_create}}}}}}}"#
    )
}

#[test]
fn top_up_adds_new_day_updates_model_and_sets_topped_up_through() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);

    write_stats_cache(
        &claude_dir,
        r#"{
            "lastComputedDate": "2026-06-10",
            "totalSessions": 5,
            "totalMessages": 100,
            "dailyActivity": [],
            "dailyModelTokens": [],
            "modelUsage": {
                "claude-opus-4": {
                    "inputTokens": 1000, "outputTokens": 500,
                    "cacheReadInputTokens": 200, "cacheCreationInputTokens": 50
                }
            },
            "hourCounts": {}
        }"#,
    );

    // Create projects/p1/sess.jsonl with a line dated AFTER lastComputedDate.
    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let jsonl_path = proj_dir.join("sess.jsonl");

    let after_line = jsonl_line(
        "2026-06-11T10:30:00+00:00",
        "claude-opus-4",
        300,
        100,
        20,
        10,
    );
    // Also a line BEFORE cutoff — must NOT be counted.
    let before_line = jsonl_line(
        "2026-06-10T23:59:59+00:00",
        "claude-opus-4",
        9999,
        9999,
        9999,
        9999,
    );
    // Line equal to cutoff — must NOT be counted.
    let equal_line = jsonl_line(
        "2026-06-10T00:00:00+00:00",
        "claude-opus-4",
        8888,
        8888,
        8888,
        8888,
    );

    {
        let mut f = std::fs::File::create(&jsonl_path).expect("create jsonl");
        writeln!(f, "{before_line}").expect("write");
        writeln!(f, "{equal_line}").expect("write");
        writeln!(f, "{after_line}").expect("write");
    }

    // Set mtime to now (definitely after cutoff 2026-06-10T00:00 UTC).
    set_mtime(&jsonl_path, SystemTime::now());

    let stats = load(&claude_dir).expect("load");

    // New day 2026-06-11 must appear in daily.
    let day11 = stats
        .daily
        .iter()
        .find(|d| d.date == "2026-06-11")
        .expect("2026-06-11 must be in daily");
    assert_eq!(day11.tokens, 400); // 300 + 100

    // Model totals grew by the after_line amounts only.
    let opus = stats
        .models
        .iter()
        .find(|m| m.model == "claude-opus-4")
        .expect("opus");
    assert_eq!(opus.input, 1300); // 1000 + 300
    assert_eq!(opus.output, 600); // 500 + 100
    assert_eq!(opus.cache_read, 220); // 200 + 20
    assert_eq!(opus.cache_create, 60); // 50 + 10

    // topped_up_through set.
    assert_eq!(stats.topped_up_through.as_deref(), Some("2026-06-11"));
}

#[test]
fn today_bucket_aggregates_todays_transcript_lines() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);

    write_stats_cache(
        &claude_dir,
        r#"{
            "lastComputedDate": "2026-06-10",
            "totalSessions": 0, "totalMessages": 0,
            "dailyActivity": [], "dailyModelTokens": [],
            "modelUsage": {}, "hourCounts": {}
        }"#,
    );

    // Today's date computed exactly as the module does (same clock).
    let today = crate::usage::epoch_secs_to_iso(crate::usage::now_epoch_secs());
    let today_date = today[..10].to_owned();
    let ts = format!("{today_date}T12:00:00+00:00");

    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let jsonl_path = proj_dir.join("sess.jsonl");
    let l1 = jsonl_line(&ts, "claude-opus-4", 100, 50, 20, 5);
    let l2 = jsonl_line(&ts, "claude-opus-4", 10, 5, 0, 0);
    std::fs::write(&jsonl_path, format!("{l1}\n{l2}\n")).expect("write");
    set_mtime(&jsonl_path, SystemTime::now());

    let stats = load(&claude_dir).expect("load");
    let today_s = stats.today.expect("today must be populated");
    assert_eq!(today_s.date, today_date);
    assert_eq!(today_s.messages, 2);
    assert_eq!(today_s.input, 110);
    assert_eq!(today_s.output, 55);
    assert_eq!(today_s.cache_read, 20);
    assert_eq!(today_s.cache_create, 5);
    assert_eq!(today_s.in_out(), 165);
    assert_eq!(today_s.total(), 190);
}

#[test]
fn top_up_skips_old_mtime_file() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);

    write_stats_cache(
        &claude_dir,
        r#"{
            "lastComputedDate": "2026-06-10",
            "totalSessions": 0, "totalMessages": 0,
            "dailyActivity": [], "dailyModelTokens": [],
            "modelUsage": {}, "hourCounts": {}
        }"#,
    );

    let proj_dir = claude_dir.join("projects").join("p2");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let jsonl_path = proj_dir.join("old.jsonl");
    let after_line = jsonl_line("2026-06-11T10:00:00+00:00", "claude-opus-4", 500, 200, 0, 0);
    std::fs::write(&jsonl_path, format!("{after_line}\n")).expect("write");

    // Set mtime to well before the cutoff (2026-01-01).
    let old_time = UNIX_EPOCH + Duration::from_secs(1_735_689_600); // 2026-01-01T00:00:00Z
    set_mtime(&jsonl_path, old_time);

    let stats = load(&claude_dir).expect("load");

    // No new days — old file was skipped by mtime guard.
    assert!(stats.daily.is_empty());
    assert!(stats.topped_up_through.is_none());
}

#[test]
fn top_up_none_when_no_last_computed_date() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);

    write_stats_cache(
        &claude_dir,
        r#"{
            "totalSessions": 0, "totalMessages": 0,
            "dailyActivity": [], "dailyModelTokens": [],
            "modelUsage": {}, "hourCounts": {}
        }"#,
    );

    let proj_dir = claude_dir.join("projects").join("p3");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let jsonl_path = proj_dir.join("sess.jsonl");
    let line = jsonl_line("2026-06-11T10:00:00+00:00", "claude-opus-4", 100, 50, 0, 0);
    std::fs::write(&jsonl_path, format!("{line}\n")).expect("write");
    set_mtime(&jsonl_path, SystemTime::now());

    let stats = load(&claude_dir).expect("load");
    // top-up skipped entirely — no last_computed_date.
    assert!(stats.topped_up_through.is_none());
    assert!(stats.daily.is_empty());
}

fn jsonl_line_with_ids(
    timestamp: &str,
    request_id: &str,
    msg_id: &str,
    model: &str,
    input: u64,
    output: u64,
) -> String {
    format!(
        r#"{{"timestamp":"{timestamp}","requestId":"{request_id}","message":{{"id":"{msg_id}","model":"{model}","usage":{{"input_tokens":{input},"output_tokens":{output},"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}}}"#
    )
}

#[test]
fn top_up_counts_nested_subagent_transcripts() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);

    write_stats_cache(
        &claude_dir,
        r#"{
            "lastComputedDate": "2026-06-10",
            "totalSessions": 0, "totalMessages": 0,
            "dailyActivity": [], "dailyModelTokens": [],
            "modelUsage": {}, "hourCounts": {}
        }"#,
    );

    // Main-session transcript: projects/p1/sess.jsonl
    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let main_path = proj_dir.join("sess.jsonl");
    let main_line = jsonl_line("2026-06-11T10:00:00+00:00", "claude-opus-4", 100, 50, 0, 0);
    std::fs::write(&main_path, format!("{main_line}\n")).expect("write main");
    set_mtime(&main_path, SystemTime::now());

    // Subagent/workflow transcript nested under <session>/subagents/.
    let sub_dir = proj_dir.join("sess").join("subagents");
    std::fs::create_dir_all(&sub_dir).expect("create subagents dir");
    let sub_path = sub_dir.join("agent-x.jsonl");
    let sub_line = jsonl_line("2026-06-11T10:05:00+00:00", "claude-opus-4", 300, 200, 0, 0);
    std::fs::write(&sub_path, format!("{sub_line}\n")).expect("write subagent");
    set_mtime(&sub_path, SystemTime::now());

    let stats = load(&claude_dir).expect("load");

    // Day total includes the nested subagent line: (100+50) + (300+200) = 650.
    let day = stats
        .daily
        .iter()
        .find(|d| d.date == "2026-06-11")
        .expect("2026-06-11 must be in daily");
    assert_eq!(day.tokens, 650);

    let opus = stats
        .models
        .iter()
        .find(|m| m.model == "claude-opus-4")
        .expect("opus");
    assert_eq!(opus.input, 400); // 100 + 300
    assert_eq!(opus.output, 250); // 50 + 200
}

#[test]
fn top_up_dedupes_same_message_across_files() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);

    write_stats_cache(
        &claude_dir,
        r#"{
            "lastComputedDate": "2026-06-10",
            "totalSessions": 0, "totalMessages": 0,
            "dailyActivity": [], "dailyModelTokens": [],
            "modelUsage": {}, "hourCounts": {}
        }"#,
    );

    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");

    // Same (requestId, message.id) mirrored into two transcripts — e.g. a forked
    // or resumed session copying a line forward. Must be counted exactly once.
    let line = jsonl_line_with_ids(
        "2026-06-11T10:00:00+00:00",
        "req_1",
        "msg_1",
        "claude-opus-4",
        100,
        50,
    );
    let f1 = proj_dir.join("a.jsonl");
    let f2 = proj_dir.join("b.jsonl");
    std::fs::write(&f1, format!("{line}\n")).expect("write a");
    std::fs::write(&f2, format!("{line}\n")).expect("write b");
    set_mtime(&f1, SystemTime::now());
    set_mtime(&f2, SystemTime::now());

    let stats = load(&claude_dir).expect("load");

    // Counted once: 100 + 50 = 150, not 300.
    let day = stats
        .daily
        .iter()
        .find(|d| d.date == "2026-06-11")
        .expect("2026-06-11 must be in daily");
    assert_eq!(day.tokens, 150);

    let opus = stats
        .models
        .iter()
        .find(|m| m.model == "claude-opus-4")
        .expect("opus");
    assert_eq!(opus.input, 100);
    assert_eq!(opus.output, 50);
}
