//! Inline tests for the Alibaba Model Studio provider — the OneConsole envelope,
//! the ratio→percent conversion, the tier→absolute-allowance lookup, and the
//! gateway/host tables. Bodies follow the real captured shapes; nothing here
//! touches the network.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

/// Wrap a payload in the gateway's success envelope. Every real response is
/// HTTP 200 and carries its verdict in these two nested `success` flags.
fn ok_envelope(payload: &str) -> String {
    format!(
        r#"{{"code":"200","data":{{"DataV2":{{"ret":["SUCCESS::"],
           "data":{{"msg":null,"code":"SUCCESS","data":{payload},"success":true}}}},
           "success":true,"httpStatus":200,"errorCode":"","api":"x","errorMsg":""}},
           "httpStatusCode":"200","requestId":"r"}}"#
    )
}

/// The dead-session answer: HTTP 200, `success:false`, and the login error code.
const NOT_LOGINED: &str = r#"{"code":"200","data":{"success":false,"httpStatus":200,
  "errorCode":"BailianGateway.Login.NotLogined","errorMsg":"not logined",
  "api":"zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/usage"},
  "httpStatusCode":"200","requestId":"r"}"#;

/// Weekly-only `/usage` — the shape the vendor removed on 2026-09-22. No
/// captured account (2026-08, 2026-10) returns it now;
/// `a_weekly_pair_is_no_longer_read` feeds this body to `stats` to pin that
/// the pair yields no bar.
const USAGE_WEEKLY_ONLY: &str = r#"{"per1WeekResetTime":1787050440000,
  "per1WeekPercentage":0.1284}"#;

/// Monthly-only `/usage` — what a real Solo account returns now.
const USAGE_MONTHLY_ONLY: &str = r#"{"per1MonthResetTime":1787050440000,
  "per1MonthPercentage":0.1284}"#;

/// A body carrying both windows: the live monthly pair and the dormant 5h pair.
const USAGE_WITH_5H: &str = r#"{"per1MonthResetTime":1787050440000,
  "per1MonthPercentage":0.1284,"per5HourResetTime":1787050440000,
  "per5HourPercentage":0.5}"#;

const SUBSCRIPTION: &str = r#"{"instanceCode":"i-1","specCode":"lite",
  "remainingDays":6,"startTime":1784000000000,"endTime":1785000000000,
  "autoRenewFlag":false,"status":"VALID"}"#;

const QUOTA_CONFIG: &str = r#"{"lite":{"five_hour":700,"monthly":11500},
  "standard":{"five_hour":3000,"monthly":45000},
  "pro":{"five_hour":12000,"monthly":180000},
  "addon_quota":{"extrabundle":20000}}"#;

fn usage(body: &str) -> UsagePayload {
    unwrap_payload(&ok_envelope(body)).expect("usage payload")
}

fn subscription() -> SubscriptionPayload {
    unwrap_payload(&ok_envelope(SUBSCRIPTION)).expect("subscription payload")
}

fn quota() -> QuotaConfig {
    unwrap_payload(&ok_envelope(QUOTA_CONFIG)).expect("quota config")
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

// ── Envelope ─────────────────────────────────────────────────────────────────

#[test]
fn not_logined_body_reads_as_console_expired() {
    // HTTP 200 carries the dead session, so a status-only reader would call
    // this a success. The distinct error keeps the scheduler from re-polling a
    // credential only a re-login can replace.
    let err: Result<UsagePayload, _> = unwrap_payload(NOT_LOGINED);
    assert!(matches!(err, Err(ThirdPartyError::ConsoleExpired)));
}

#[test]
fn other_gateway_failures_stay_ordinary_status_errors() {
    let body = r#"{"code":"200","data":{"success":false,"httpStatus":200,
      "errorCode":"FAIL_SYS_INTERNAL_ERROR","errorMsg":"boom"},
      "httpStatusCode":"200"}"#;
    let err: Result<UsagePayload, _> = unwrap_payload(body);
    assert!(matches!(err, Err(ThirdPartyError::Status)));
}

// ── Percentages ──────────────────────────────────────────────────────────────

#[test]
fn monthly_ratio_is_scaled_to_a_percentage() {
    // `per1MonthPercentage` is a RATIO in 0..1 despite the name: 0.1284 = 12.84%.
    let s = stats(&usage(USAGE_MONTHLY_ONLY), None, None);
    assert_eq!(s.bars.len(), 1);
    assert!(
        close(s.bars[0].pct, 12.84),
        "expected 12.84%, got {}",
        s.bars[0].pct
    );
}

#[test]
fn a_ratio_above_one_clamps_to_a_full_bar() {
    let s = stats(&usage(r#"{"per1MonthPercentage":1.5}"#), None, None);
    assert!(close(s.bars[0].pct, 100.0), "got {}", s.bars[0].pct);
}

#[test]
fn a_monthly_only_body_yields_no_5h_bar() {
    // The 5h pair is dormant: every tier's `quota-config` still publishes a
    // `five_hour` allowance (measured 2026-10-14), so the pair may return, but
    // a real Solo account returns the monthly pair alone — a synthesised 5h bar
    // would be a claim clauth cannot make.
    let s = stats(&usage(USAGE_MONTHLY_ONLY), None, None);
    assert_eq!(
        s.bars.iter().map(|b| b.label.as_str()).collect::<Vec<_>>(),
        vec!["30d"]
    );
    assert_eq!(
        s.bars[0].resets_at.as_deref(),
        Some("2026-08-18T10:54:00+00:00"),
        "the reset stamp is epoch MILLIseconds",
    );
}

/// The one captured account (an international Solo account, probed 2026-08
/// and 2026-10) never returned a 5h pair: the 5h window is dormant — every tier's
/// `quota-config` still publishes a `five_hour` allowance (measured 2026-10-14),
/// so the pair may return — yet `/usage` carries the monthly pair alone. This
/// test is therefore the only thing standing between the 5h path and a
/// dead-code sweep, and the field returning would be a silent regression
/// without it. Do not delete it as coverage of an impossible case.
#[test]
fn a_body_carrying_both_windows_orders_the_5h_first() {
    let s = stats(&usage(USAGE_WITH_5H), None, None);
    assert_eq!(
        s.bars.iter().map(|b| b.label.as_str()).collect::<Vec<_>>(),
        vec!["5h", "30d"]
    );
    assert!(close(s.bars[0].pct, 50.0), "got {}", s.bars[0].pct);
}

// ── Tier allowances ──────────────────────────────────────────────────────────

#[test]
fn the_tier_quota_turns_percentages_into_absolutes() {
    let s = stats(&usage(USAGE_WITH_5H), Some(&subscription()), Some(&quota()));
    let five_hour = &s.bars[0];
    let monthly = &s.bars[1];
    assert_eq!(five_hour.total, Some(700.0), "lite five_hour");
    assert_eq!(monthly.total, Some(11500.0), "lite monthly");
    // 11500 × 0.1284 = 1476.6: allowance × reported fraction.
    assert!(
        close(monthly.used.unwrap(), 1476.6),
        "used is the allowance times the reported fraction, got {:?}",
        monthly.used
    );
}

#[test]
fn an_unknown_tier_leaves_the_bar_without_absolutes() {
    // No subscription read (or a tier the config doesn't publish) must not
    // invent a ceiling — the percentage still stands on its own.
    let s = stats(&usage(USAGE_MONTHLY_ONLY), None, Some(&quota()));
    assert_eq!(s.bars[0].total, None);
    assert_eq!(s.bars[0].used, None);
}

// ── Subscription rows ────────────────────────────────────────────────────────

#[test]
fn subscription_yields_plan_status_and_remaining_days() {
    let s = stats(&usage(USAGE_MONTHLY_ONLY), Some(&subscription()), None);
    assert_eq!(s.plan.as_deref(), Some("lite"));
    let rows: Vec<(&str, &str)> = s
        .rows
        .iter()
        .map(|r| (r.label.as_str(), r.value.as_str()))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("subscription", ""),
            ("status", "valid"),
            ("remaining", "6 days"),
        ]
    );
}

#[test]
fn a_non_valid_subscription_status_renders_as_danger() {
    let sub: SubscriptionPayload = unwrap_payload(&ok_envelope(
        r#"{"specCode":"lite","status":"EXPIRED","remainingDays":0}"#,
    ))
    .unwrap();
    let s = stats(&usage(USAGE_MONTHLY_ONLY), Some(&sub), None);
    let status = s.rows.iter().find(|r| r.label == "status").unwrap();
    assert_eq!(status.kind, StatRowKind::Danger);
    // Still "available": the same api key keeps billing pay-as-you-go, so
    // claiming the account can't call the API would overclaim.
    assert!(s.is_available);
}

// ── Hosts + gateway ──────────────────────────────────────────────────────────

#[test]
fn every_known_alibaba_host_is_recognised() {
    for url in [
        "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic",
        "https://token-plan.cn-beijing.maas.aliyuncs.com/apps/anthropic",
        "https://coding-intl.dashscope.aliyuncs.com/apps/anthropic",
        "https://coding.dashscope.aliyuncs.com/apps/anthropic",
        "https://token-plan.maas.qwencloudapi.com/apps/anthropic",
    ] {
        assert!(matches_base_url(url), "{url}");
    }
}

#[test]
fn a_host_extension_never_claims_the_provider() {
    // A bare prefix match would send the console token to someone else's host.
    assert!(!matches_base_url(
        "https://coding.dashscope.aliyuncs.com.evil.tld/apps/anthropic"
    ));
    assert!(!matches_base_url("https://api.anthropic.com"));
}

#[test]
fn the_base_url_decides_the_console_front_and_region() {
    assert_eq!(
        site_and_region("https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic"),
        Some((ConsoleSite::International, "ap-southeast-1"))
    );
    assert_eq!(
        site_and_region("https://coding.dashscope.aliyuncs.com/apps/anthropic"),
        Some((ConsoleSite::Domestic, "cn-beijing"))
    );
    assert_eq!(site_and_region("https://api.z.ai/api/anthropic"), None);
}

#[test]
fn each_region_and_site_pair_picks_its_own_gateway() {
    assert_eq!(
        gateway("cn-beijing", ConsoleSite::Domestic),
        Gateway {
            origin: "https://bailian-cs.console.aliyun.com",
            action: "BroadScopeAspnGateway",
        }
    );
    assert_eq!(
        gateway("cn-beijing", ConsoleSite::International),
        Gateway {
            origin: "https://bailian-cs.console.alibabacloud.com",
            action: "BroadScopeAspnGateway",
        }
    );
    assert_eq!(
        gateway("ap-southeast-1", ConsoleSite::Domestic),
        Gateway {
            origin: "https://modelstudio-cs.console.aliyun.com",
            action: "IntlBroadScopeAspnGateway",
        }
    );
    assert_eq!(
        gateway("ap-southeast-1", ConsoleSite::International),
        Gateway {
            origin: "https://bailian-singapore-cs.alibabacloud.com",
            action: "IntlBroadScopeAspnGateway",
        }
    );
}

#[test]
fn an_unknown_region_falls_back_to_the_cn_beijing_row_of_its_own_site() {
    // Concrete origins on both arms, NOT `gateway(unknown, s) == gateway("cn-beijing", s)`:
    // that shape evaluates the same match arm on both sides, so a site-blind
    // fallback — the one bug this test exists to catch — satisfies it.
    assert_eq!(
        gateway("eu-central-1", ConsoleSite::International),
        Gateway {
            origin: "https://bailian-cs.console.alibabacloud.com",
            action: "BroadScopeAspnGateway",
        },
        "an unknown region keeps its INTERNATIONAL front",
    );
    assert_eq!(
        gateway("eu-central-1", ConsoleSite::Domestic),
        Gateway {
            origin: "https://bailian-cs.console.aliyun.com",
            action: "BroadScopeAspnGateway",
        },
        "and an unknown region keeps its DOMESTIC front",
    );
}

// ── Request shape ────────────────────────────────────────────────────────────

#[test]
fn the_params_json_carries_the_two_mandatory_blocks() {
    // Dropping either `V` or `cornerstoneParam` answers `Bad Request`.
    let params: serde_json::Value =
        serde_json::from_str(&request_params("zeldaHttp.apikeyMgr./x")).unwrap();
    assert_eq!(params["Api"], "zeldaHttp.apikeyMgr./x");
    assert_eq!(params["V"], "1.0");
    let cs = &params["Data"]["cornerstoneParam"];
    assert_eq!(cs["protocol"], "V2");
    assert_eq!(cs["console"], "ONE_CONSOLE");
    assert_eq!(cs["productCode"], "p_efm");
    assert_eq!(cs["switchUserType"], 3);
    // Hardcoded upstream for every region and front alike — NOT derived from
    // the profile's `ConsoleSite`, which names a different axis.
    assert_eq!(cs["consoleSite"], "BAILIAN_ALIYUN");
}

#[test]
fn a_missing_console_credential_is_the_same_state_as_a_dead_one() {
    // No credential, no request: the fetch resolves without touching the
    // network, so the profile is suppressed rather than polled forever.
    assert!(matches!(fetch(None), Err(ThirdPartyError::ConsoleExpired)));
}

#[test]
fn the_throttle_key_follows_the_credential_not_the_provider() {
    let intl = ConsoleCredential {
        token: "t".to_string(),
        site: ConsoleSite::International,
        region: "ap-southeast-1".to_string(),
    };
    assert_eq!(
        gateway_origin(Some(&intl)),
        "https://bailian-singapore-cs.alibabacloud.com"
    );
    // With no credential nothing is ever requested; the default row is a stable
    // placeholder rather than a claim about where this profile would go.
    assert_eq!(
        gateway_origin(None),
        "https://bailian-cs.console.aliyun.com"
    );
}

#[test]
fn a_zero_or_negative_allowance_row_is_no_allowance() {
    // A published-but-empty row would otherwise render `0 / 0` with a
    // `used: Some(0.0)`, which reads as a measured zero rather than "unknown".
    let cfg: QuotaConfig = unwrap_payload(&ok_envelope(
        r#"{"lite":{"five_hour":0,"monthly":-1},"pro":{"monthly":40000}}"#,
    ))
    .unwrap();
    assert_eq!(cfg.allowance("lite", "five_hour"), None);
    assert_eq!(cfg.allowance("lite", "monthly"), None);
    assert_eq!(cfg.allowance("pro", "monthly"), Some(40000.0));

    let sub: SubscriptionPayload =
        unwrap_payload(&ok_envelope(r#"{"specCode":"lite","status":"VALID"}"#)).unwrap();
    let s = stats(&usage(USAGE_MONTHLY_ONLY), Some(&sub), Some(&cfg));
    assert_eq!(s.bars[0].total, None, "an empty row is not a ceiling");
    assert_eq!(s.bars[0].used, None);
}

/// A tier the quota config does not publish drops the absolutes and keeps the
/// bars — deliberately quiet, because it is an EXPECTED state, not a defect:
/// `specCode` admits `max` while the observed `quota-config` publishes only
/// lite/essential/standard/pro, so failing the fetch would delete a real
/// subscriber's windows over a missing enrichment.
#[test]
fn a_tier_the_quota_config_omits_keeps_its_bars() {
    let sub: SubscriptionPayload =
        unwrap_payload(&ok_envelope(r#"{"specCode":"max","status":"VALID"}"#)).unwrap();
    let s = stats(&usage(USAGE_MONTHLY_ONLY), Some(&sub), Some(&quota()));
    assert_eq!(s.plan.as_deref(), Some("max"));
    assert_eq!(s.bars.len(), 1, "the window still renders");
    assert!(close(s.bars[0].pct, 12.84));
    assert_eq!(s.bars[0].total, None, "no ceiling is invented for it");
    assert_eq!(s.bars[0].used, None);
}

// ── console_url ───────────────────────────────────────────────────────────────

/// Each endpoint resolves to its OWN console page. Token Plan and Coding Plan
/// are separate products, the two fronts spell their routes differently, and
/// Qwen Cloud's Token Plan lives on Qwen Cloud's own console, so a page
/// borrowed from a sibling row lands an operator on a plan they do not hold.
/// Exact values: every one is a vendor-published deep link, and nothing here
/// may be extrapolated from its neighbour.
#[test]
fn each_endpoint_resolves_to_its_own_console_page() {
    let cases = [
        (
            "https://token-plan.ap-southeast-1.maas.aliyuncs.com",
            "https://modelstudio.console.alibabacloud.com/ap-southeast-1?tab=plan#/efm/subscription/overview",
        ),
        (
            "https://token-plan.cn-beijing.maas.aliyuncs.com",
            "https://bailian.console.aliyun.com/cn-beijing?tab=plan#/efm/subscription/overview",
        ),
        (
            "https://coding-intl.dashscope.aliyuncs.com",
            "https://modelstudio.console.alibabacloud.com/ap-southeast-1/?tab=globalset#/efm/coding_plan",
        ),
        (
            "https://coding.dashscope.aliyuncs.com",
            "https://bailian.console.aliyun.com/cn-beijing/?tab=plan#/efm/subscription/coding-plan",
        ),
        (
            "https://token-plan.maas.qwencloudapi.com",
            "https://home.qwencloud.com/api-keys",
        ),
    ];
    for (base_url, page) in cases {
        assert_eq!(console_url(base_url), Some(page), "for {base_url}");
    }
    let pages: std::collections::BTreeSet<_> = cases.iter().map(|(_, page)| *page).collect();
    assert_eq!(pages.len(), cases.len(), "no two endpoints share a page");
}

/// The console page is reachable for exactly the URLs the provider claims, so
/// the offer and the fetch never disagree about which endpoints are Alibaba.
#[test]
fn console_url_tracks_matches_base_url_in_both_directions() {
    let path = "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic";
    assert!(matches_base_url(path));
    assert!(console_url(path).is_some(), "a path suffix still matches");

    for rejected in [
        "https://token-plan.ap-southeast-1.maas.aliyuncs.com.evil.tld",
        "https://api.deepseek.com",
        "",
    ] {
        assert!(!matches_base_url(rejected), "{rejected} is not Alibaba");
        assert_eq!(console_url(rejected), None, "so it has no page: {rejected}");
    }
}

// ── Qwen Cloud Token Plan (monthly quota) ─────────────────────────────────────

/// `…/api/v2/usage` as the intl gateway answered it for a Qwen Cloud Token Plan
/// account on 2026-10-14 (bytes captured on the `qwen` account, request ids
/// kept): the monthly pair alone, no weekly and no 5h pair.
const CAPTURED_MONTHLY_USAGE: &str = r#"{
	"code":"200",
	"data":{
		"DataV2":{
			"ret":[
				"SUCCESS::接口调用成功"
			],
			"data":{
				"msg":"Success.",
				"code":"SUCCESS",
				"data":{
					"per1MonthPercentage":0.000095790974,
					"per1MonthResetTime":1794412800000
				},
				"requestId":"5e6e48df-365e-9ba0-b2bc-1841929a56e6",
				"success":true
			}
		},
		"success":true,
		"httpStatus":200,
		"errorCode":"",
		"api":"zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/usage",
		"errorMsg":""
	},
	"httpStatusCode":"200",
	"requestId":"5e6e48df-365e-4ba0-b2bc-1841929a56e6",
	"successResponse":true
}"#;

/// `…/api/v2/quota-config` from the same capture: per-tier `five_hour` +
/// `monthly`, no `weekly` row left.
const CAPTURED_MONTHLY_QUOTA_CONFIG: &str = r#"{
	"code":"200",
	"data":{
		"DataV2":{
			"ret":[
				"SUCCESS::接口调用成功"
			],
			"data":{
				"msg":"Success.",
				"code":"SUCCESS",
				"data":{
					"standard":{
						"five_hour":3000.0,
						"monthly":45000.0
					},
					"addon_quota":{
						"extrabundle":20000.0
					},
					"lite":{
						"five_hour":700.0,
						"monthly":11500.0
					},
					"pro":{
						"five_hour":12000.0,
						"monthly":180000.0
					},
					"essential":{
						"five_hour":1800.0,
						"monthly":25500.0
					}
				},
				"requestId":"e07e9c3a-07fc-9abe-9b09-49ae9d6da9f7",
				"success":true
			}
		},
		"success":true,
		"httpStatus":200,
		"errorCode":"",
		"api":"zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/quota-config",
		"errorMsg":""
	},
	"httpStatusCode":"200",
	"requestId":"e07e9c3a-07fc-4abe-9b09-49ae9d6da9f7",
	"successResponse":true
}"#;

/// `…/api/v2/subscription` from the same capture (its instance code redacted
/// at capture time).
const CAPTURED_SUBSCRIPTION: &str = r#"{
	"code":"200",
	"data":{
		"DataV2":{
			"ret":[
				"SUCCESS::接口调用成功"
			],
			"data":{
				"msg":"Success.",
				"code":"SUCCESS",
				"data":{
					"instanceCode":"sfm_tokenplansolo_public_intl-REDACTED",
					"specCode":"lite",
					"remainingDays":28,
					"startTime":1786445271000,
					"endTime":1794412800000,
					"autoRenewFlag":false,
					"status":"VALID"
				},
				"requestId":"14375a4b-9d28-940e-b51b-ec88033cc90c",
				"success":true
			}
		},
		"success":true,
		"httpStatus":200,
		"errorCode":"",
		"api":"zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/subscription",
		"errorMsg":""
	},
	"httpStatusCode":"200",
	"requestId":"14375a4b-9d28-440e-b51b-ec88033cc90c",
	"successResponse":true
}"#;

/// The captured monthly payloads render as ONE `30d` bar: the ratio scaled to
/// a percentage (0.000095790974 → 0.0095790974%), the reset read as epoch
/// milliseconds (1794412800000 → 2026-11-11T16:00:00Z, the captured
/// subscription's `endTime`), and the lite tier's `monthly` allowance (11500)
/// turning it into `used / total` (11500 × 0.000095790974 = 1.101596201).
#[test]
fn the_captured_monthly_usage_reads_as_one_30d_bar() {
    let usage: UsagePayload = unwrap_payload(CAPTURED_MONTHLY_USAGE).expect("usage payload");
    let quota: QuotaConfig =
        unwrap_payload(CAPTURED_MONTHLY_QUOTA_CONFIG).expect("quota config payload");
    let sub: SubscriptionPayload =
        unwrap_payload(CAPTURED_SUBSCRIPTION).expect("subscription payload");
    let s = stats(&usage, Some(&sub), Some(&quota));
    assert_eq!(s.plan.as_deref(), Some("lite"));
    assert_eq!(
        s.bars.iter().map(|b| b.label.as_str()).collect::<Vec<_>>(),
        vec!["30d"]
    );
    let bar = &s.bars[0];
    assert!(
        (bar.pct - 0.009_579_097_4).abs() < 1e-12,
        "expected 0.0095790974%, got {}",
        bar.pct
    );
    assert_eq!(bar.resets_at.as_deref(), Some("2026-11-11T16:00:00+00:00"));
    assert_eq!(bar.total, Some(11500.0), "lite monthly");
    assert!(
        (bar.used.unwrap() - 1.101_596_201).abs() < 1e-9,
        "got {:?}",
        bar.used
    );
}

/// The weekly window was removed on 2026-09-22 and no captured payload carries
/// it any more, so a `per1Week*` pair is no longer read: a body holding only
/// that pair yields no bar rather than a stale `7d` one.
#[test]
fn a_weekly_pair_is_no_longer_read() {
    let s = stats(&usage(USAGE_WEEKLY_ONLY), None, None);
    assert!(s.bars.is_empty(), "got {:?}", s.bars);
}

/// Qwen Cloud's single Token Plan host is Alibaba's: the intl console session
/// reads its quota (measured 2026-10-14), so it resolves to the international
/// front and region, and its console page is Qwen Cloud's API Keys page, which
/// holds both the Individual and the Team Token Plan keys.
#[test]
fn the_qwen_cloud_token_plan_host_is_alibaba_international() {
    let url = "https://token-plan.maas.qwencloudapi.com/apps/anthropic";
    assert!(matches_base_url(url));
    assert_eq!(
        crate::providers::Provider::from_base_url(url),
        Some(crate::providers::Provider::Alibaba)
    );
    assert_eq!(
        site_and_region(url),
        Some((ConsoleSite::International, "ap-southeast-1"))
    );
    assert_eq!(
        console_url(url),
        Some("https://home.qwencloud.com/api-keys")
    );
    for rejected in [
        "https://token-plan.maas.qwencloudapi.com.evil.tld/apps/anthropic",
        // pay-as-you-go: the Token Plan key is refused there, so it has no plan
        // quota to read.
        "https://maas.qwencloudapi.com/apps/anthropic",
    ] {
        assert!(
            !matches_base_url(rejected),
            "{rejected} is not the Token Plan host"
        );
    }
}
