//! Inline tests for the DeepSeek provider — wire-shape parsing and the
//! response → display-rows mapping.

use super::*;

#[test]
fn deepseek_response_parses_wire_shape() {
    // Shape per https://api-docs.deepseek.com/api/get-user-balance
    let json = r#"{
        "is_available": true,
        "balance_infos": [{
            "currency": "USD",
            "total_balance": "110.00",
            "granted_balance": "10.00",
            "topped_up_balance": "100.00"
        }]
    }"#;
    let raw: DeepSeekResponse = serde_json::from_str(json).expect("parse balance response");
    assert!(raw.is_available);
    assert_eq!(raw.balance_infos.len(), 1);
    assert_eq!(raw.balance_infos[0].currency, "USD");
}

#[test]
fn stats_builds_heading_and_body_rows() {
    let raw = DeepSeekResponse {
        is_available: true,
        balance_infos: vec![DeepSeekBalance {
            currency: "USD".to_string(),
            total_balance: "110.00".to_string(),
            granted_balance: "10.00".to_string(),
            topped_up_balance: "100.00".to_string(),
        }],
    };
    let stats = stats(&raw);
    assert!(stats.is_available);
    assert_eq!(stats.rows.len(), 4);
    assert_eq!(stats.rows[0].kind, StatRowKind::Heading);
    assert_eq!(stats.rows[0].label, "USD balance");
    assert_eq!(stats.rows[1].label, "total");
    assert_eq!(stats.rows[1].value, "110.00 USD");
    assert!(stats.rows[1..].iter().all(|r| r.kind == StatRowKind::Body));
}

#[test]
fn stats_unavailable_carries_danger_row() {
    let raw = DeepSeekResponse {
        is_available: false,
        balance_infos: vec![],
    };
    let stats = stats(&raw);
    assert!(!stats.is_available);
    assert_eq!(stats.rows.len(), 1);
    assert_eq!(stats.rows[0].kind, StatRowKind::Danger);
    assert!(stats.rows[0].label.is_empty());
}

#[test]
fn stats_available_but_empty_yields_no_rows() {
    let raw = DeepSeekResponse {
        is_available: true,
        balance_infos: vec![],
    };
    let stats = stats(&raw);
    assert!(stats.is_available);
    assert!(stats.rows.is_empty());
}
