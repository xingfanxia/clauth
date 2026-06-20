//! Inline tests for the Z.ai provider — `quota/limit` → bars/plan/detail-rows
//! and `model-usage.totalUsage` → token rows, plus the count formatter. Parsed
//! against the real captured wire shapes (giant hourly arrays trimmed).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

// Real `/api/monitor/usage/quota/limit` shape (3 limits, TIME_LIMIT carries
// absolutes + a per-web-tool breakdown; the two TOKENS_LIMIT are percentage-only).
const QUOTA: &str = r#"{
  "code":200,"msg":"Operation successful","success":true,
  "data":{
    "limits":[
      {"type":"TIME_LIMIT","unit":5,"number":1,"usage":1000,"currentValue":250,
       "remaining":750,"percentage":25,"nextResetTime":1784489490994,
       "usageDetails":[
         {"modelCode":"search-prime","usage":3},
         {"modelCode":"web-reader","usage":0},
         {"modelCode":"zread","usage":1}]},
      {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":0},
      {"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":12,"nextResetTime":1782502290996}
    ],
    "level":"pro"
  }
}"#;

#[test]
fn quota_yields_bars_plan_and_detail_rows() {
    let env: ZaiEnvelope<QuotaData> = serde_json::from_str(QUOTA).unwrap();
    assert!(env.success);
    let stats = quota_stats(&env.data);

    assert_eq!(stats.plan.as_deref(), Some("pro"));
    assert!(!stats.best_effort, "typed provider is not best-effort");

    // Three bars, source order, API labels (no inferred 5h/7d vocabulary).
    assert_eq!(stats.bars.len(), 3);
    assert_eq!(stats.bars[0].label, "time limit");
    assert_eq!(stats.bars[1].label, "tokens limit");
    assert_eq!(stats.bars[2].label, "tokens limit");

    // TIME_LIMIT absolutes: currentValue → used, currentValue+remaining → total.
    assert_eq!(stats.bars[0].used, Some(250.0));
    assert_eq!(stats.bars[0].total, Some(1000.0));
    assert!(stats.bars[0].resets_at.is_some());

    // Percentage-only token bar carries no absolutes.
    assert!(stats.bars[1].used.is_none() && stats.bars[1].total.is_none());

    // Per-web-tool breakdown surfaces only the non-empty TIME_LIMIT block, with a
    // heading + one row per tool.
    let labels: Vec<&str> = stats.rows.iter().map(|r| r.label.as_str()).collect();
    assert!(labels.contains(&"search-prime"));
    assert!(labels.contains(&"zread"));
    let search = stats
        .rows
        .iter()
        .find(|r| r.label == "search-prime")
        .unwrap();
    assert_eq!(search.value, "3");
}

#[test]
fn quota_error_envelope_rejected() {
    // The 200-with-success:false envelope z.ai returns for unknown routes.
    let env: ZaiEnvelope<QuotaData> =
        serde_json::from_str(r#"{"code":500,"msg":"404 NOT_FOUND","success":false}"#).unwrap();
    assert!(
        !env.success,
        "fetch() turns this into ThirdPartyError::Status"
    );
}

#[test]
fn model_usage_yields_token_rows() {
    let total = ModelTotalUsage {
        total_model_call_count: 451.0,
        total_tokens_usage: 52_245_057.0,
        model_summary_list: vec![
            ModelSummary {
                model_name: "GLM-5.2".to_string(),
                total_tokens: 44_456_531.0,
            },
            ModelSummary {
                model_name: "GLM-4.7".to_string(),
                total_tokens: 7_788_526.0,
            },
        ],
    };
    let rows = model_rows(&total);
    // Heading + 2 models + total.
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0].label, "7d tokens");
    assert_eq!(rows[1].label, "GLM-5.2");
    assert_eq!(rows[1].value, "44.5M");
    assert_eq!(rows[3].label, "total");
    assert!(rows[3].value.contains("52.2M"));
    assert!(rows[3].value.contains("451 calls"));
}

#[test]
fn model_usage_all_zero_yields_no_rows() {
    let total = ModelTotalUsage {
        total_model_call_count: 0.0,
        total_tokens_usage: 0.0,
        model_summary_list: vec![ModelSummary {
            model_name: "GLM-5.2".to_string(),
            total_tokens: 0.0,
        }],
    };
    assert!(
        model_rows(&total).is_empty(),
        "an empty window adds nothing"
    );
}

#[test]
fn fmt_count_scales_units() {
    assert_eq!(fmt_count(451.0), "451");
    assert_eq!(fmt_count(7_788_526.0), "7.8M");
    assert_eq!(fmt_count(52_245_057.0), "52.2M");
    assert_eq!(fmt_count(1_500.0), "1.5k");
    assert_eq!(fmt_count(0.0), "0");
    assert_eq!(fmt_count(-5.0), "0");
}

#[test]
fn zai_date_param_is_space_separated_utc() {
    // epoch_secs_to_iso → "2026-06-20T00:32:07+00:00"; the date param drops the
    // timezone and replaces T with a space.
    let secs = crate::usage::iso_to_epoch_secs("2026-06-20T00:32:07+00:00").unwrap();
    assert_eq!(secs_to_zai_date(secs), "2026-06-20 00:32:07");
    assert_eq!(
        url_encode("2026-06-20 00:32:07"),
        "2026-06-20%2000%3A32%3A07"
    );
}

#[test]
fn matches_base_url_recognises_zai() {
    assert!(matches_base_url("https://api.z.ai/api/anthropic"));
    assert!(matches_base_url("https://api.z.ai"));
    assert!(!matches_base_url("https://api.z.ai.evil.tld"));
    assert!(!matches_base_url("https://api.deepseek.com"));
}
