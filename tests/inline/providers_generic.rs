//! Inline tests for the generic usage engine — the JSON scanner
//! (bars/rows/plan) and error-envelope rejection. `fetch` hits the network and
//! is exercised manually, not here.

use super::*;

// Real z.ai `/api/monitor/usage/quota/limit` shape (trimmed).
const ZAI_QUOTA: &str = r#"{
    "code":200,"msg":"Operation successful","success":true,
    "data":{"level":"pro","limits":[
        {"type":"TIME_LIMIT","percentage":0,"nextResetTime":1784489490994,
         "usage":1000,"currentValue":0,"remaining":1000},
        {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":1,"nextResetTime":1781915527377}
    ]}
}"#;

#[test]
fn scan_zai_quota_shape_yields_bars_and_plan() {
    let value: serde_json::Value = serde_json::from_str(ZAI_QUOTA).unwrap();
    assert!(!is_error_envelope(&value));

    let (plan, bars, rows) = scan(&value);
    assert_eq!(plan.as_deref(), Some("pro"));
    assert_eq!(bars.len(), 2);
    assert_eq!(bars[0].label, "time limit");
    assert_eq!(bars[0].pct, 0.0);
    assert!(bars[0].resets_at.is_some());
    // Absolute amounts: `currentValue` → used, `used + remaining` → total (no
    // explicit ceiling field). Rendered as the bar's trailing `x / y`.
    assert_eq!(bars[0].used, Some(0.0));
    assert_eq!(bars[0].total, Some(1000.0));
    assert_eq!(bars[1].label, "tokens limit");
    assert_eq!(bars[1].pct, 1.0);
    // Percentage-only limit carries no absolute amounts.
    assert!(bars[1].used.is_none() && bars[1].total.is_none());
    assert!(rows.is_empty(), "bars present → no scalar rows harvested");
}

#[test]
fn scan_zai_200_error_envelope_is_rejected() {
    // z.ai returns this 200 body for unknown routes — must not parse as empty usage.
    let value: serde_json::Value =
        serde_json::from_str(r#"{"code":500,"msg":"404 NOT_FOUND","success":false}"#).unwrap();
    assert!(is_error_envelope(&value));
    let (plan, bars, rows) = scan(&value);
    assert!(plan.is_none() && bars.is_empty() && rows.is_empty());
}

#[test]
fn scan_scalar_balance_shape_yields_rows_not_bars() {
    // A provider returning balances (no percentages) → text rows.
    let body = r#"{"is_available":true,"balance_infos":[
        {"currency":"USD","total_balance":12.5,"granted_balance":5.0,"topped_up_balance":7.5}
    ]}"#;
    let value: serde_json::Value = serde_json::from_str(body).unwrap();
    assert!(!is_error_envelope(&value));

    let (plan, bars, rows) = scan(&value);
    assert!(bars.is_empty(), "no percentage field → no bars");
    assert!(plan.is_none());
    let values: Vec<&str> = rows.iter().map(|r| r.value.as_str()).collect();
    assert!(values.contains(&"12.50"));
    assert!(values.contains(&"7.50"));
    assert!(values.contains(&"5"));
}

#[test]
fn humanize_label_handles_cases() {
    assert_eq!(humanize_label("TIME_LIMIT"), "time limit");
    assert_eq!(humanize_label("modelCode"), "model code");
    assert_eq!(humanize_label("total_balance"), "total balance");
}
