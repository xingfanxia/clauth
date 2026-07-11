//! OAUTH-1 — deterministic, network-free tests for the PKCE + URL machinery,
//! plus the loopback callback's security paths over in-process sockets.
//! The browser round-trip and token exchange are manual acceptance.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use super::{
    authorize_url, base64url_nopad, challenge_from_verifier, handle_callback, percent_decode,
    percent_encode, query_param, request_target, wait_for_code,
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

// ── handle_callback / wait_for_code: the redirect's security paths ────────────

/// Feed one request line through `handle_callback` over a real loopback socket
/// pair; returns its verdict and the raw HTTP response the "browser" received.
fn callback_roundtrip(
    request_line: &str,
    expected_state: &str,
) -> (anyhow::Result<Option<String>>, String) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let mut client = TcpStream::connect(addr).expect("connect");
    client
        .write_all(request_line.as_bytes())
        .expect("send request");
    let (server, _) = listener.accept().expect("accept");
    let verdict = handle_callback(server, expected_state);
    // handle_callback dropped its stream → EOF, so this reads the full response.
    let mut response = String::new();
    client.read_to_string(&mut response).expect("read response");
    (verdict, response)
}

#[test]
fn callback_accepts_matching_state_and_extracts_the_code() {
    let (verdict, response) = callback_roundtrip(
        "GET /callback?code=authcode-1&state=STATE HTTP/1.1\r\n",
        "STATE",
    );
    assert_eq!(
        verdict.expect("valid callback").as_deref(),
        Some("authcode-1")
    );
    assert!(response.starts_with("HTTP/1.1 200 OK"), "got: {response}");
}

#[test]
fn callback_aborts_on_a_state_mismatch() {
    let (verdict, response) = callback_roundtrip(
        "GET /callback?code=authcode-1&state=EVIL HTTP/1.1\r\n",
        "STATE",
    );
    let err = verdict
        .expect_err("a state mismatch must abort the login")
        .to_string();
    assert!(err.contains("state mismatch"), "err was: {err}");
    assert!(response.starts_with("HTTP/1.1 400"), "got: {response}");
}

#[test]
fn callback_aborts_on_an_oauth_error_param() {
    let (verdict, response) = callback_roundtrip(
        "GET /callback?error=access_denied&error_description=denied HTTP/1.1\r\n",
        "STATE",
    );
    let err = verdict
        .expect_err("an OAuth error param is fatal")
        .to_string();
    assert!(err.contains("authorization failed"), "err was: {err}");
    assert!(response.starts_with("HTTP/1.1 400"), "got: {response}");
}

#[test]
fn callback_ignores_unrelated_requests_and_keeps_waiting() {
    // A favicon probe is answered 404 and is NOT fatal — the login keeps waiting.
    let (verdict, response) = callback_roundtrip("GET /favicon.ico HTTP/1.1\r\n", "STATE");
    assert!(verdict.expect("non-callback path is ignored").is_none());
    assert!(response.starts_with("HTTP/1.1 404"), "got: {response}");

    // /callback with no code at all → 400, still not fatal.
    let (verdict, response) = callback_roundtrip("GET /callback?state=STATE HTTP/1.1\r\n", "STATE");
    assert!(verdict.expect("missing code is ignored").is_none());
    assert!(response.starts_with("HTTP/1.1 400"), "got: {response}");
}

#[test]
fn callback_pages_are_distinct_and_never_reflect_query_values() {
    // Success: styled page that attempts a tab auto-close.
    let (_, ok) = callback_roundtrip("GET /callback?code=c&state=STATE HTTP/1.1\r\n", "STATE");
    assert!(ok.contains("You're logged in"), "got: {ok}");
    assert!(
        ok.contains("window.close"),
        "the success page attempts a tab auto-close"
    );

    // A user-declined consent reads as canceled, not broken — and the
    // attacker-controllable error_description never reaches the HTML.
    let (_, denied) = callback_roundtrip(
        "GET /callback?error=access_denied&error_description=evil-marker HTTP/1.1\r\n",
        "STATE",
    );
    assert!(denied.contains("Login canceled"), "got: {denied}");
    assert!(
        !denied.contains("evil-marker"),
        "untrusted query values must never be reflected into the page"
    );

    // Any other OAuth error keeps the generic failure page, with no auto-close.
    let (_, failed) = callback_roundtrip("GET /callback?error=server_error HTTP/1.1\r\n", "STATE");
    assert!(failed.contains("Login failed"), "got: {failed}");
    assert!(
        !failed.contains("window.close"),
        "only the success page auto-closes"
    );
}

#[test]
fn wait_for_code_times_out_at_the_deadline() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let err = wait_for_code(&listener, "STATE", std::time::Instant::now())
        .expect_err("an already-passed deadline must bail")
        .to_string();
    assert!(err.contains("timed out"), "err was: {err}");
}
