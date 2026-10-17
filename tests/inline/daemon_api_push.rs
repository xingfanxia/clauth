#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Web Push routes: the VAPID public key, one subscription per device at the
//! view tier, the vendor-host allowlist, the default event set, and revoke
//! dropping the device's row.

#![cfg(unix)]

use super::*;

use base64::Engine as _;

use crate::daemon::api::devices::{self, Tier};
use crate::profile::{AppConfig, AppState};
use crate::testutil::{HomeSandbox, OTHER_TOKEN, body_json, call, ctx_with, req, seed_device};

const SUB_KEYS: &str = r#""keys":{"p256dh":"BNcRdreALRFXTkOOUHK1EtK2wtaz5Ry4YfYCA_0QTpQtUbVlUls0VJXg7A8u-Ts1XbjhazAkj7I99e8QcYP7DkM","auth":"tBHItJI5svbpez7KI4CCXg"}"#;

fn ctx() -> std::sync::Arc<ApiContext> {
    let config = std::sync::Arc::new(crate::lockorder::RankedMutex::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    }));
    let ctx = ctx_with(config);
    seed_device("phone", Tier::View, OTHER_TOKEN);
    ctx
}

fn put(ctx: &ApiContext, body: &str) -> Response {
    call(
        ctx,
        &req("PUT", "/api/v1/push/subscription", Some(OTHER_TOKEN), body),
    )
}

fn delete(ctx: &ApiContext) -> Response {
    call(
        ctx,
        &req("DELETE", "/api/v1/push/subscription", Some(OTHER_TOKEN), ""),
    )
}

fn subscription(endpoint: &str, events: Option<&str>) -> String {
    match events {
        Some(events) => format!(r#"{{"endpoint":"{endpoint}",{SUB_KEYS},"events":{events}}}"#),
        None => format!(r#"{{"endpoint":"{endpoint}",{SUB_KEYS}}}"#),
    }
}

#[test]
fn a_view_device_reads_one_stable_uncompressed_p256_public_key() {
    let _home = HomeSandbox::new();
    let ctx = ctx();

    let first = call(&ctx, &req("GET", "/api/v1/push/key", Some(OTHER_TOKEN), ""));
    let second = call(&ctx, &req("GET", "/api/v1/push/key", Some(OTHER_TOKEN), ""));

    assert_eq!(first.status, 200);
    let key = body_json(&first)["public_key"]
        .as_str()
        .expect("public_key is a string")
        .to_string();
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(&key)
        .expect("public_key is unpadded base64url");
    assert_eq!(raw.len(), 65, "an uncompressed P-256 point is 65 bytes");
    assert_eq!(raw[0], 0x04, "an uncompressed point opens with 0x04");
    assert_eq!(body_json(&second)["public_key"], key.as_str());
}

#[test]
fn an_endpoint_outside_the_vendor_push_hosts_is_refused() {
    let _home = HomeSandbox::new();
    let ctx = ctx();

    for endpoint in [
        "https://169.254.169.254/latest/meta-data",
        "https://127.0.0.1:8443/api/v1/status",
        "http://fcm.googleapis.com/fcm/send/abc",
        "https://fcm.googleapis.com.evil.example/fcm/send/abc",
        "https://evil.example/fcm.googleapis.com",
        "https://user@evil.example@fcm.googleapis.com/fcm/send/abc",
    ] {
        let resp = put(&ctx, &subscription(endpoint, None));
        assert_eq!(
            (resp.status, body_json(&resp)["error"].clone()),
            (400, serde_json::json!("push_endpoint_not_allowed")),
            "{endpoint}"
        );
    }
}

#[test]
fn each_vendor_push_host_is_accepted() {
    let _home = HomeSandbox::new();
    let ctx = ctx();

    for endpoint in [
        "https://fcm.googleapis.com/fcm/send/abc",
        "https://web.push.apple.com/QGuQyavXutnMH",
        "https://updates.push.services.mozilla.com/wpush/v2/gAAAAA",
    ] {
        let resp = put(&ctx, &subscription(endpoint, None));
        assert_eq!(resp.status, 200, "{endpoint}");
    }
}

#[test]
fn a_subscription_defaults_to_blocked_and_account_and_a_re_put_replaces_it() {
    let _home = HomeSandbox::new();
    let ctx = ctx();
    let endpoint = "https://fcm.googleapis.com/fcm/send/abc";

    let defaulted = put(&ctx, &subscription(endpoint, None));
    assert_eq!(defaulted.status, 200);
    assert_eq!(
        body_json(&defaulted)["events"],
        serde_json::json!(["blocked", "account"])
    );

    let replaced = put(
        &ctx,
        &subscription(endpoint, Some(r#"["window","blocked"]"#)),
    );
    assert_eq!(replaced.status, 200);
    assert_eq!(
        body_json(&replaced)["events"],
        serde_json::json!(["blocked", "window"])
    );
}

#[test]
fn an_unknown_event_name_is_refused() {
    let _home = HomeSandbox::new();
    let ctx = ctx();

    let resp = put(
        &ctx,
        &subscription(
            "https://fcm.googleapis.com/fcm/send/abc",
            Some(r#"["prompt"]"#),
        ),
    );
    assert_eq!(
        (resp.status, body_json(&resp)["error"].clone()),
        (400, serde_json::json!("bad_request"))
    );
}

#[test]
fn delete_drops_the_calling_devices_row_and_a_second_delete_finds_none() {
    let _home = HomeSandbox::new();
    let ctx = ctx();
    assert_eq!(
        put(
            &ctx,
            &subscription("https://fcm.googleapis.com/fcm/send/abc", None)
        )
        .status,
        200
    );

    assert_eq!(delete(&ctx).status, 200);
    let again = delete(&ctx);
    assert_eq!(
        (again.status, body_json(&again)["error"].clone()),
        (404, serde_json::json!("no_push_subscription"))
    );
}

#[test]
fn revoking_a_device_drops_its_subscription() {
    let _home = HomeSandbox::new();
    let ctx = ctx();
    assert_eq!(
        put(
            &ctx,
            &subscription("https://fcm.googleapis.com/fcm/send/abc", None)
        )
        .status,
        200
    );

    devices::revoke("phone").expect("revoke the phone");
    seed_device("phone", Tier::View, OTHER_TOKEN);

    let resp = delete(&ctx);
    assert_eq!(
        (resp.status, body_json(&resp)["error"].clone()),
        (404, serde_json::json!("no_push_subscription"))
    );
}
