//! OAUTH-1 — deterministic, network-free tests for the PKCE + URL machinery.
//! The browser round-trip and token exchange are manual acceptance.

use super::{
    authorize_url, base64url_nopad, challenge_from_verifier, percent_decode, percent_encode,
    query_param, request_target,
};

#[test]
fn pkce_challenge_matches_rfc7636_vector() {
    // RFC 7636 Appendix B: verifier → S256 challenge.
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    assert_eq!(
        challenge_from_verifier(verifier),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

#[test]
fn base64url_nopad_matches_rfc4648_vectors() {
    // RFC 4648 §10 test vectors (base64url == base64 for these ASCII inputs).
    assert_eq!(base64url_nopad(b""), "");
    assert_eq!(base64url_nopad(b"f"), "Zg");
    assert_eq!(base64url_nopad(b"fo"), "Zm8");
    assert_eq!(base64url_nopad(b"foo"), "Zm9v");
    assert_eq!(base64url_nopad(b"foob"), "Zm9vYg");
    assert_eq!(base64url_nopad(b"fooba"), "Zm9vYmE");
    assert_eq!(base64url_nopad(b"foobar"), "Zm9vYmFy");
    // Exercises the URL-safe alphabet (`-` and `_` instead of `+`/`/`).
    assert_eq!(base64url_nopad(&[0xfb, 0xff, 0xbf]), "-_-_");
}

#[test]
fn percent_encode_and_decode_round_trip() {
    let scope = "user:profile user:inference";
    assert_eq!(percent_encode(scope), "user%3Aprofile%20user%3Ainference");
    assert_eq!(percent_decode(&percent_encode(scope)), scope);

    let redirect = "http://localhost:52341/callback";
    assert_eq!(percent_decode(&percent_encode(redirect)), redirect);
}

#[test]
fn query_param_extracts_and_decodes() {
    let q = "code=abc%2D123&state=xyz";
    assert_eq!(query_param(q, "code").as_deref(), Some("abc-123"));
    assert_eq!(query_param(q, "state").as_deref(), Some("xyz"));
    assert_eq!(query_param(q, "missing"), None);
}

#[test]
fn percent_decode_survives_malformed_and_multibyte() {
    // A bare '%' followed by a multi-byte UTF-8 char must not panic (byte-index
    // slicing would); the '%' passes through and the char is preserved.
    assert_eq!(percent_decode("%€"), "%€");
    // Trailing '%' with no hex digits, and a non-hex escape, pass through raw.
    assert_eq!(percent_decode("a%"), "a%");
    assert_eq!(percent_decode("a%zz"), "a%zz");
    // Valid escapes still decode.
    assert_eq!(percent_decode("%2Fpath"), "/path");
}

#[test]
fn request_target_pulls_path_and_query() {
    assert_eq!(
        request_target("GET /callback?code=x&state=y HTTP/1.1"),
        Some("/callback?code=x&state=y")
    );
    assert_eq!(request_target(""), None);
}

#[test]
fn authorize_url_uses_claude_ai_host_code_true_and_six_scopes() {
    let url = authorize_url("http://localhost:1234/callback", "CHAL", "STATE");
    // Pro/Max subscription authorize host (CLAUDE_AI_AUTHORIZE_URL), NOT the
    // platform.claude.com Console host which doesn't mint claude.ai creds.
    assert!(url.starts_with("https://claude.com/cai/oauth/authorize?"));
    // code=true is appended unconditionally, as the binary does for every request.
    assert!(url.contains("code=true"));
    assert!(url.contains(&format!("client_id={}", crate::oauth::CLIENT_ID)));
    assert!(url.contains("response_type=code"));
    assert!(url.contains("code_challenge=CHAL"));
    assert!(url.contains("code_challenge_method=S256"));
    assert!(url.contains("state=STATE"));
    assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1234%2Fcallback"));
    // The full 6-scope union (ALL_OAUTH_SCOPES), percent-encoded.
    assert!(url.contains(
        "scope=org%3Acreate_api_key%20user%3Aprofile%20user%3Ainference%20user%3Asessions%3Aclaude_code%20user%3Amcp_servers%20user%3Afile_upload"
    ));
}
