//! Web Push delivery: which state changes become a push, RFC 8291 payload
//! encryption, the RFC 8292 VAPID JWT, and the POST to the device's push
//! service.
//!
//! Three triggers, each fired on a transition and never on a re-read of the
//! same state: a herdr pane moving INTO `blocked`, an account entering a failed
//! login or key state, and a chain member's 5h or 7d window reaching the line
//! the fallback chain switches it at. The latches are in-process like the 5h bell's,
//! so a daemon restart can announce a standing state once more, where a
//! persisted latch would swallow one after a crash.
//!
//! Delivery runs on its own thread off a bounded queue, so neither the
//! daemon's tick nor the herdr follower ever waits on a push service. A push
//! goes only to a row whose device the device list still holds under the
//! pairing that registered it, whose endpoint still passes the push-host
//! allowlist at send, and whose event set names the event; each event kind's
//! own cap bounds the outbound rate whatever the triggers do. The payload is
//! the event kind and the account name, nothing else.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use ring::rand::{SecureRandom, SystemRandom};
use ring::{aead, agreement, hkdf};

use crate::daemon::api::panes::PaneProbe;
use crate::daemon::api::push::{
    Dropped, PushEvent, Subscription, VapidKey, allowed_origin, deliverable, drop_if_endpoint,
    vapid_key,
};
use crate::logline::logline;

/// The VAPID `sub` claim: the contact a push service reaches the sender at.
/// The project's own page rather than anyone's mailbox.
pub(crate) const VAPID_SUBJECT: &str = "https://github.com/uwuclxdy/clauth";
/// How long one VAPID JWT is valid: half of RFC 8292's 24-hour ceiling, and
/// a fresh token is signed for every push.
const JWT_LIFETIME: Duration = Duration::from_secs(12 * 60 * 60);
/// The aes128gcm record size the header declares. The payload is a few dozen
/// bytes, so one record always holds it.
const RECORD_SIZE: u32 = 4096;
/// The outbound cap: at most this many POSTs per [`CAP_WINDOW`] per event
/// kind, across every device. A flapping agent or a churn of account states can
/// mint pushes faster than a phone is worth waking, and a push service
/// throttles a sender that floods it.
const CAP_POSTS: usize = 30;
const CAP_WINDOW: Duration = Duration::from_secs(10 * 60);
/// One POST's whole budget, connect to last body byte.
const POST_TIMEOUT: Duration = Duration::from_secs(15);
/// Triggers queued while the sender is busy. A full queue drops the newest:
/// by then the cap is what decides anyway.
const QUEUE_DEPTH: usize = 64;

/// What happened, before it is addressed to anyone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Trigger {
    /// A herdr pane moved into `blocked`. The account is resolved at send,
    /// off the daemon's tick.
    Blocked { pane_id: String },
    /// An account entered a failed login or key state.
    Account { profile: String },
    /// A chain member's 5h or 7d window reached its chain line.
    Window { profile: String },
}

impl Trigger {
    fn event(&self) -> PushEvent {
        match self {
            Self::Blocked { .. } => PushEvent::Blocked,
            Self::Account { .. } => PushEvent::Account,
            Self::Window { .. } => PushEvent::Window,
        }
    }

    /// How long a push service holds the push for an offline phone, and how
    /// hard it wakes one: an agent waiting on the user is urgent and stale
    /// within the hour; a failing login stays true until someone fixes it.
    fn ttl_and_urgency(&self) -> (u32, &'static str) {
        match self {
            Self::Blocked { .. } => (60 * 60, "high"),
            Self::Account { .. } => (24 * 60 * 60, "normal"),
            Self::Window { .. } => (60 * 60, "normal"),
        }
    }
}

/// The whole payload: the event kind, and the account name when one is known.
#[derive(serde::Serialize)]
struct Payload<'a> {
    event: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<&'a str>,
}

pub(crate) fn payload(event: PushEvent, account: Option<&str>) -> Vec<u8> {
    serde_json::to_vec(&Payload {
        event: event.as_str(),
        account,
    })
    .unwrap_or_else(|_| format!(r#"{{"event":"{}"}}"#, event.as_str()).into_bytes())
}

// ------------------------------------------------------------------ triggers

/// One account's state as a published status body read it, projected by
/// the daemon's status writer.
pub(crate) struct Reading<'a> {
    pub(crate) name: &'a str,
    /// A login or key the account depends on is failing: a refresh was
    /// rejected as revoked or invalid, or a provider rejected the usage
    /// credential.
    pub(crate) failing: bool,
    /// The 5h window: the chain's switch line for this account (`None` off
    /// the chain) and the utilization read (`None` when the reading has none).
    pub(crate) five_hour: WindowReading,
    /// The 7d window against the member's weekly line.
    pub(crate) seven_day: WindowReading,
    /// A `Fresh`, trusted reading: only one moves the window latches.
    pub(crate) fresh: bool,
}

/// One window of a [`Reading`]: the line it is judged at and its
/// utilization, each `None` when absent.
pub(crate) struct WindowReading {
    pub(crate) line: Option<f64>,
    pub(crate) util: Option<f64>,
}

/// The account and window latches the daemon feeds each published status
/// body.
#[derive(Default)]
pub(crate) struct StatusLatches {
    /// Accounts already announced as failing, until they read healthy again.
    failing: HashSet<String>,
    /// Accounts already announced past each window's chain line: the 5h
    /// bell's own latch, judged on the chain's lines instead of the bell's.
    five_hour: crate::bell::BellLatch,
    seven_day: crate::bell::BellLatch,
}

impl StatusLatches {
    /// The triggers one published body fires.
    pub(crate) fn observe<'a>(
        &mut self,
        readings: impl IntoIterator<Item = Reading<'a>>,
    ) -> Vec<Trigger> {
        let readings: Vec<Reading<'a>> = readings.into_iter().collect();
        let mut fired = Vec::new();
        for reading in &readings {
            if !reading.failing {
                self.failing.remove(reading.name);
            } else if self.failing.insert(reading.name.to_string()) {
                fired.push(Trigger::Account {
                    profile: reading.name.to_string(),
                });
            }
            let five_hour = self.five_hour.observe(
                reading.name,
                reading.five_hour.line,
                reading.five_hour.util,
                reading.fresh,
            );
            let seven_day = self.seven_day.observe(
                reading.name,
                reading.seven_day.line,
                reading.seven_day.util,
                reading.fresh,
            );
            // One push for the body: the payload names the account, not the
            // window, so a second would tell the phone nothing new.
            if five_hour.is_some() || seven_day.is_some() {
                fired.push(Trigger::Window {
                    profile: reading.name.to_string(),
                });
            }
        }
        fired
    }

    /// Forget every latch entry — `failing` plus both window latches — whose
    /// account is not in `roster`, so an account deleted and re-created under
    /// the same name announces again. Callers pass the ROSTER (the full account
    /// set), never the published body: a disabled account is hidden from the
    /// body but is still an account, so its latches must survive the hide.
    pub(crate) fn forget_absent<'a>(&mut self, roster: impl IntoIterator<Item = &'a str>) {
        let roster: HashSet<&str> = roster.into_iter().collect();
        self.failing.retain(|name| roster.contains(name.as_str()));
        self.five_hour.forget_absent(&roster);
        self.seven_day.forget_absent(&roster);
    }
}

/// Which panes were last seen `blocked`. Kept across herdr reconnects, so a
/// snapshot repeating a standing `blocked` fires nothing.
#[derive(Default)]
pub(crate) struct BlockedLatch {
    blocked: HashMap<String, bool>,
}

/// herdr's agent status for a pane waiting on its user.
const BLOCKED: &str = "blocked";

impl BlockedLatch {
    /// One pane's status changed: `true` when it moved into `blocked`.
    pub(crate) fn changed(&mut self, pane_id: &str, status: &str) -> bool {
        let now = status == BLOCKED;
        let was = self.blocked.insert(pane_id.to_string(), now);
        now && was != Some(true)
    }

    /// Every pane's status, from a (re)connect: the panes that moved into
    /// `blocked` since last seen. A pane missing from the snapshot is gone and
    /// forgotten.
    pub(crate) fn snapshot(&mut self, panes: &[(String, String)]) -> Vec<String> {
        let present: HashSet<&str> = panes.iter().map(|(pane, _)| pane.as_str()).collect();
        self.blocked
            .retain(|pane, _| present.contains(pane.as_str()));
        panes
            .iter()
            .filter(|(pane, status)| self.changed(pane, status))
            .map(|(pane, _)| pane.clone())
            .collect()
    }
}

// -------------------------------------------------------------- encryption

/// An HKDF output length, the shape `ring` wants it in.
struct Len(usize);

impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn expand(prk: &hkdf::Prk, info: &[&[u8]], out: &mut [u8]) -> Result<()> {
    prk.expand(info, Len(out.len()))
        .and_then(|okm| okm.fill(out))
        .map_err(|_| anyhow!("HKDF expansion failed"))
}

/// RFC 8291: `plaintext` encrypted to the browser key `ua_public` and its
/// `auth_secret`, as one RFC 8188 `aes128gcm` message (salt, record size,
/// the ephemeral public key, then the record). The ephemeral key is drawn
/// from `ephemeral_rng` and the salt is passed in, so a test reproduces the
/// RFC's vector; production passes the OS CSPRNG and a fresh salt.
pub(crate) fn encrypt(
    ua_public: &[u8],
    auth_secret: &[u8],
    plaintext: &[u8],
    ephemeral_rng: &dyn SecureRandom,
    salt: [u8; 16],
) -> Result<Vec<u8>> {
    // The record carries the plaintext, the last-record delimiter and the tag.
    if plaintext.len() + 1 + aead::AES_128_GCM.tag_len() > RECORD_SIZE as usize {
        bail!("the push payload does not fit one record");
    }
    let private = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, ephemeral_rng)
        .map_err(|_| anyhow!("CSPRNG failure minting the push's ephemeral key"))?;
    let as_public = private
        .compute_public_key()
        .map_err(|_| anyhow!("the push's ephemeral public key did not compute"))?;
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, ua_public);

    let mut ikm = [0u8; 32];
    agreement::agree_ephemeral(private, &peer, |ecdh_secret| {
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, auth_secret).extract(ecdh_secret);
        expand(
            &prk,
            &[b"WebPush: info\0", ua_public, as_public.as_ref()],
            &mut ikm,
        )
    })
    .map_err(|_| anyhow!("the subscription's p256dh is not a P-256 point"))??;

    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, &salt).extract(&ikm);
    let mut cek = [0u8; 16];
    expand(&prk, &[b"Content-Encoding: aes128gcm\0"], &mut cek)?;
    let mut nonce = [0u8; 12];
    expand(&prk, &[b"Content-Encoding: nonce\0"], &mut nonce)?;

    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_128_GCM, &cek)
            .map_err(|_| anyhow!("the content key did not load"))?,
    );
    let mut record = Vec::with_capacity(plaintext.len() + 1 + aead::AES_128_GCM.tag_len());
    record.extend_from_slice(plaintext);
    // The delimiter of the last (here the only) record, with no padding.
    record.push(0x02);
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(nonce),
        aead::Aad::empty(),
        &mut record,
    )
    .map_err(|_| anyhow!("the push payload did not seal"))?;

    let key_id = as_public.as_ref();
    let mut message = Vec::with_capacity(16 + 4 + 1 + key_id.len() + record.len());
    message.extend_from_slice(&salt);
    message.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    message.push(u8::try_from(key_id.len()).map_err(|_| anyhow!("key id too long"))?);
    message.extend_from_slice(key_id);
    message.extend_from_slice(&record);
    Ok(message)
}

// ----------------------------------------------------------------- vapid

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[derive(serde::Serialize)]
struct Claims<'a> {
    aud: &'a str,
    exp: u64,
    sub: &'static str,
}

/// RFC 8292's `Authorization` value for one push to `origin`: an ES256 JWT
/// whose audience is the push service's origin, valid [`JWT_LIFETIME`] from
/// `now_secs`, and the key that verifies it.
pub(crate) fn vapid_authorization(key: &VapidKey, origin: &str, now_secs: u64) -> Result<String> {
    let header = b64url(br#"{"typ":"JWT","alg":"ES256"}"#);
    let claims = serde_json::to_vec(&Claims {
        aud: origin,
        exp: now_secs + JWT_LIFETIME.as_secs(),
        sub: VAPID_SUBJECT,
    })
    .context("failed to encode the VAPID claims")?;
    let signing_input = format!("{header}.{}", b64url(&claims));
    let signature = key.sign(signing_input.as_bytes())?;
    Ok(format!(
        "vapid t={signing_input}.{}, k={}",
        b64url(&signature),
        key.public_key_b64()
    ))
}

// -------------------------------------------------------------- delivery

/// One outbound request, built in full before any transport sees it.
pub(crate) struct Post {
    pub(crate) url: String,
    pub(crate) headers: Vec<(&'static str, String)>,
    pub(crate) body: Vec<u8>,
}

impl std::fmt::Debug for Post {
    /// The header names only: the URL is the device's endpoint and the
    /// `Authorization` value is a signed token.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| *name).collect();
        f.debug_struct("Post")
            .field("headers", &names)
            .field("body_len", &self.body.len())
            .finish_non_exhaustive()
    }
}

/// How a POST goes out: the status the push service answered, or the kind of
/// failure that kept it from answering. A field rather than a global, so a
/// test drives the whole delivery with no listener.
pub(crate) type Transport = fn(&Post) -> std::result::Result<u16, &'static str>;

/// The real transport: the crate's shared HTTP agent, bounded per request.
pub(crate) fn ureq_transport(post: &Post) -> std::result::Result<u16, &'static str> {
    send_with_agent(crate::usage::http_agent(), post)
}

/// The one request-config path, shared by production and the no-redirect test:
/// `agent` supplies the transport and its trust store (production's shared one,
/// a test's loopback-trusting one). https only, no redirect followed
/// (`max_redirects(0)` hands a 3xx back as its status), TLS verification as the
/// agent has it, on.
fn send_with_agent(agent: &ureq::Agent, post: &Post) -> std::result::Result<u16, &'static str> {
    let mut request = agent.post(&post.url);
    for (name, value) in &post.headers {
        request = request.header(*name, value);
    }
    request
        .config()
        .https_only(true)
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(POST_TIMEOUT))
        .build()
        .send(&post.body[..])
        .map(|response| response.status().as_u16())
        .map_err(|e| failure_kind(&e))
}

/// A failure named by its kind alone: ureq's own text can quote the URL.
fn failure_kind(e: &ureq::Error) -> &'static str {
    match e {
        ureq::Error::Timeout(_) => "timeout",
        ureq::Error::HostNotFound => "dns",
        ureq::Error::ConnectionFailed => "connect",
        ureq::Error::RequireHttpsOnly(_) => "https_only",
        ureq::Error::Io(_) => "io",
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) => "tls",
        _ => "other",
    }
}

/// One event kind's outbound cap: a sliding window of its own, so a flood of
/// one kind leaves the other kinds' budgets untouched.
pub(crate) struct RateCap {
    kind: PushEvent,
    sent: VecDeque<Instant>,
    limit: usize,
    window: Duration,
    /// Whether this full stretch has been logged.
    noted: bool,
}

impl RateCap {
    pub(crate) fn new(kind: PushEvent, limit: usize, window: Duration) -> Self {
        Self {
            kind,
            sent: VecDeque::new(),
            limit,
            window,
            noted: false,
        }
    }

    /// Whether one more POST may go at `now`, counting it when it may.
    pub(crate) fn admit(&mut self, now: Instant) -> bool {
        while self
            .sent
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= self.window)
        {
            self.sent.pop_front();
        }
        if self.sent.len() >= self.limit {
            if !self.noted {
                self.noted = true;
                logline!(
                    "clauth push: {} {} pushes in the last {} minutes; dropping {} pushes until \
                     the rate falls",
                    self.limit,
                    self.kind.as_str(),
                    self.window.as_secs() / 60,
                    self.kind.as_str()
                );
            }
            return false;
        }
        self.noted = false;
        self.sent.push_back(now);
        true
    }
}

/// The sending half: resolves a trigger to its rows and posts to each.
pub(crate) struct Sender {
    transport: Transport,
    /// Resolves a blocked pane to the profile it spends.
    herdr_probe: PaneProbe,
    blocked: RateCap,
    account: RateCap,
    window: RateCap,
}

impl Sender {
    pub(crate) fn new(
        transport: Transport,
        herdr_probe: PaneProbe,
        posts: usize,
        window: Duration,
    ) -> Self {
        Self {
            transport,
            herdr_probe,
            blocked: RateCap::new(PushEvent::Blocked, posts, window),
            account: RateCap::new(PushEvent::Account, posts, window),
            window: RateCap::new(PushEvent::Window, posts, window),
        }
    }

    /// The trigger's kind's own cap.
    fn cap_mut(&mut self, event: PushEvent) -> &mut RateCap {
        match event {
            PushEvent::Blocked => &mut self.blocked,
            PushEvent::Account => &mut self.account,
            PushEvent::Window => &mut self.window,
        }
    }

    /// Push `trigger` to every row that asked for its event.
    pub(crate) fn deliver(&mut self, trigger: &Trigger) {
        let event = trigger.event();
        let rows = match deliverable(event) {
            Ok(rows) => rows,
            Err(e) => {
                logline!(
                    "clauth push: no {} push sent, the subscription list did not read: {}",
                    event.as_str(),
                    crate::daemon::api::http::sanitize_for_log(&format!("{e:#}"))
                );
                return;
            }
        };
        if rows.is_empty() {
            return;
        }
        let key = match vapid_key() {
            Ok(key) => key,
            Err(e) => {
                logline!(
                    "clauth push: no {} push sent, the VAPID key did not read: {}",
                    event.as_str(),
                    crate::daemon::api::http::sanitize_for_log(&format!("{e:#}"))
                );
                return;
            }
        };
        let account = match trigger {
            Trigger::Blocked { pane_id } => {
                crate::daemon::api::panes::pane_profile(&self.herdr_probe, pane_id)
            }
            Trigger::Account { profile } | Trigger::Window { profile } => Some(profile.clone()),
        };
        let body = payload(event, account.as_deref());
        for row in &rows {
            self.post(&key, row, trigger, &body);
        }
    }

    fn post(&mut self, key: &VapidKey, row: &Subscription, trigger: &Trigger, body: &[u8]) {
        let device = crate::daemon::api::http::sanitize_for_log(&row.device);
        // The allowlist again, at send: a row reaches the store through the
        // route's check, but the file is the user's to edit.
        let Some(origin) = allowed_origin(&row.endpoint) else {
            logline!(
                "clauth push: device '{device}' holds an endpoint off the push-host allowlist; \
                 not pushing to it"
            );
            return;
        };
        let post = match build_post(key, row, &origin, trigger, body) {
            Ok(post) => post,
            Err(e) => {
                logline!(
                    "clauth push: no push for device '{device}': {}",
                    crate::daemon::api::http::sanitize_for_log(&format!("{e:#}"))
                );
                return;
            }
        };
        if !self.cap_mut(trigger.event()).admit(Instant::now()) {
            return;
        }
        match (self.transport)(&post) {
            Ok(200..=299) => {}
            Ok(status @ (404 | 410)) => match drop_if_endpoint(&row.device, &row.endpoint) {
                Ok(Dropped::Row) => logline!(
                    "clauth push: the push service of device '{device}' answered {status}, so \
                     its subscription is gone; dropped it"
                ),
                Ok(Dropped::Replaced) => logline!(
                    "clauth push: the push service of device '{device}' answered {status} for \
                     an endpoint the device has since replaced; kept the new one"
                ),
                Ok(Dropped::NoRow) => logline!(
                    "clauth push: the push service of device '{device}' answered {status}; the \
                     device holds no subscription any more"
                ),
                Err(e) => logline!(
                    "clauth push: the push service of device '{device}' answered {status}, but \
                     its subscription could not be dropped: {}",
                    crate::daemon::api::http::sanitize_for_log(&format!("{e:#}"))
                ),
            },
            Ok(status) => {
                logline!("clauth push: the push service of device '{device}' answered {status}")
            }
            Err(kind) => logline!("clauth push: push to device '{device}' failed: {kind}"),
        }
    }
}

/// The encrypted, signed request for one row.
fn build_post(
    key: &VapidKey,
    row: &Subscription,
    origin: &str,
    trigger: &Trigger,
    body: &[u8],
) -> Result<Post> {
    let ua_public = crate::daemon::api::push::decode_b64url(&row.p256dh)
        .context("the subscription's p256dh is not base64url")?;
    let auth = crate::daemon::api::push::decode_b64url(&row.auth)
        .context("the subscription's auth secret is not base64url")?;
    let rng = SystemRandom::new();
    let mut salt = [0u8; 16];
    rng.fill(&mut salt)
        .map_err(|_| anyhow!("CSPRNG failure drawing the push salt"))?;
    let encrypted = encrypt(&ua_public, &auth, body, &rng, salt)?;
    let (ttl, urgency) = trigger.ttl_and_urgency();
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    Ok(Post {
        url: row.endpoint.clone(),
        headers: vec![
            ("Authorization", vapid_authorization(key, origin, now_secs)?),
            ("Content-Encoding", "aes128gcm".to_string()),
            ("Content-Type", "application/octet-stream".to_string()),
            ("TTL", ttl.to_string()),
            ("Urgency", urgency.to_string()),
        ],
        body: encrypted,
    })
}

/// Where the daemon hands its triggers: the sending thread's inbox.
#[derive(Clone)]
pub(crate) struct Queue(SyncSender<Trigger>);

impl Queue {
    /// Queue `trigger` without waiting. A full queue or a gone sender drops it
    /// with a line: no caller may park on a push.
    pub(crate) fn send(&self, trigger: Trigger) {
        match self.0.try_send(trigger) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                logline!("clauth push: the push queue is full; dropping a push");
            }
            Err(TrySendError::Disconnected(_)) => {
                logline!("clauth push: the push sender is gone; dropping a push");
            }
        }
    }
}

/// Start the sending thread for the daemon's life.
pub(crate) fn spawn(herdr_probe: PaneProbe) -> Result<Queue> {
    let (tx, rx) = std::sync::mpsc::sync_channel(QUEUE_DEPTH);
    let sender = Sender::new(ureq_transport, herdr_probe, CAP_POSTS, CAP_WINDOW);
    std::thread::Builder::new()
        .name("clauth-push".to_string())
        .spawn(move || run(sender, &rx))
        .context("failed to start the push sender")?;
    Ok(Queue(tx))
}

fn run(mut sender: Sender, inbox: &Receiver<Trigger>) {
    for trigger in inbox {
        sender.deliver(&trigger);
    }
}

#[cfg(test)]
pub(crate) fn queue_for_tests() -> (Queue, Receiver<Trigger>) {
    let (tx, rx) = std::sync::mpsc::sync_channel(QUEUE_DEPTH);
    (Queue(tx), rx)
}

#[cfg(test)]
#[path = "../tests/inline/push.rs"]
mod tests;
