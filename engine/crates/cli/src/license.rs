//! `drums activate <key>` — the one paid boundary in the product, verified
//! entirely offline.
//!
//! # What is free, and what this gates
//!
//! Everything local is free forever: detect, attribute, reproduce, repair,
//! verify, propose. The user's machine, the user's coding-agent CLI, the
//! user's API key. Nothing in this module is consulted anywhere on that
//! path, and [`crate::engine`] does not depend on it at all.
//!
//! The paid capability is **act-alone authority** — letting Drums ship a
//! failure class without waiting for a person. `engine-authority` decides
//! when a class has *earned* that (five consecutive clean ships), and
//! `drums authority promote <class>` is the human action that applies it.
//! That command is the paywall, and deliberately the only one: the product
//! proves it deserves autonomy against the customer's own incidents first,
//! and only then asks for money.
//!
//! # Why verification is offline, with no license server
//!
//! A key is an Ed25519 signature over a small JSON payload, checked against
//! a public key compiled into this binary. There is no license server, no
//! activation call, no heartbeat, and no network I/O of any kind in this
//! file.
//!
//! Two reasons, both of which would survive an argument with a buyer:
//!
//! 1. **A product that stops working when our server is down is a product
//!    nobody in a regulated environment will deploy.** Drums sits in the
//!    path of production repair. A customer whose repairs stall because
//!    *our* availability slipped has been handed our outage as their
//!    incident, and no amount of uptime promising makes that acceptable at
//!    procurement.
//! 2. **Phoning home to check a license contradicts the telemetry posture.**
//!    The whole local-mode pitch is that nothing leaves the boundary. A
//!    call-out carrying a customer identifier on every run is exactly the
//!    egress a security review is looking for, and it would be indefensible
//!    to argue "we move the record, not the material" while the binary
//!    beacons.
//!
//! The cost of this choice is honest: an offline key cannot be revoked
//! mid-term. That is what the expiry field is for, and why keys are issued
//! for a bounded period rather than forever.
//!
//! # Failing closed
//!
//! A key that is malformed, unparseable, signed by the wrong key, or past
//! its expiry grants NOTHING — it never degrades to "probably fine". It also
//! never takes anything away: an invalid key leaves the user exactly where
//! they were with no key at all, which is the whole free product.
//! [`LicenseStatus::Invalid`] and [`LicenseStatus::Expired`] both carry the
//! specific reason, and every refusal prints it, because "your license is
//! invalid" with no further detail is the single most infuriating message a
//! paid tool can produce.
//!
//! # Issuing keys (founder-side)
//!
//! The private key must NEVER be in this repository, in a build, or in CI.
//! It lives wherever the founder keeps secrets, and is used only to mint.
//! The round trip is [`generate_issuing_keypair`] once, then [`mint`] per
//! customer — see `a_minted_key_verifies_and_the_private_half_never_leaves_this_test`
//! at the bottom of this file for the exact three lines.

use std::path::PathBuf;

use base64::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::record_cmd::relative_when;

/// Every key starts with this. Version is in the prefix, not the payload
/// alone, so a key from a future scheme is rejected by shape before a single
/// byte of it is trusted.
pub const KEY_PREFIX: &str = "drums-license-v1.";

/// The placeholder that means "this build was never given an issuing key".
///
/// Left in source on purpose. A real public key is a release-engineering
/// input, not something a working copy should carry: see
/// [`ISSUING_PUBKEY_HEX`]. While it is this value, every key fails closed
/// with a message that says exactly that — which is correct, because a build
/// with no trust anchor genuinely cannot verify anything.
const UNPROVISIONED: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// The trust anchor: the public half of the issuing keypair, hex-encoded.
///
/// Set at build time with `DRUMS_LICENSE_PUBKEY=<64 hex chars> cargo build
/// --release`, or paste the value over [`UNPROVISIONED`] below. Nothing else
/// in the product reads it, and it is a PUBLIC key — committing it leaks
/// nothing.
const ISSUING_PUBKEY_HEX: &str = match option_env!("DRUMS_LICENSE_PUBKEY") {
    Some(k) => k,
    None => UNPROVISIONED,
};

/// The URL a refusal points at. `/#pricing` is a real section of the real
/// landing page (`website/site/index.html`) — no invented path, and no
/// invented price, because the pricing decision is "book a call" until the
/// numbers are signed off (`docs/PRICING.md`).
pub const TALK_URL: &str = "https://drums.sh/#pricing";

/// What a key asserts. Signed as a whole; no field is trusted individually.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct License {
    /// Always 1 for [`KEY_PREFIX`]. Checked, so a v2 payload smuggled into a
    /// v1 envelope is refused rather than half-understood.
    pub v: u32,
    /// Who it was issued to. Printed back on `drums activate` so a customer
    /// can see at a glance that they pasted the right key.
    pub customer: String,
    /// `pilot` / `team` / `private` (`docs/PRICING.md`). Deliberately a
    /// plain string and deliberately NOT used to gate anything today: every
    /// paid tier includes act-alone, and encoding a capability matrix here
    /// would mean a tier name we add next year bricks a key that is
    /// otherwise perfectly valid.
    pub tier: String,
    pub issued_ms: u64,
    /// Unix ms. Past this, the key is [`LicenseStatus::Expired`] and grants
    /// nothing — this is the only revocation an offline scheme has.
    pub expires_ms: u64,
}

/// What this machine's license situation actually is. There is no "probably
/// licensed" state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LicenseStatus {
    /// No key anywhere. The overwhelmingly common case, and a completely
    /// fine one — it is the entire free product.
    Absent,
    /// Verified against the compiled-in issuing key, and not yet expired.
    Active(License),
    /// Verified, but past `expires_ms`. Fails closed, and says when.
    Expired(License),
    /// Present and unusable. `why` is the specific reason, never a shrug.
    Invalid { why: String },
}

impl LicenseStatus {
    /// The one question the rest of the binary asks. Only [`Self::Active`].
    pub fn grants_act_alone(&self) -> bool {
        matches!(self, LicenseStatus::Active(_))
    }
}

// -- verification (pure: no clock, no filesystem, no network) -----------------

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

/// Decode exactly 32 hex bytes. Rejects anything else rather than padding or
/// truncating — a half-typed anchor must not silently become a valid-looking
/// one.
fn from_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = [0u8; 32];
    for (i, pair) in bytes.chunks_exact(2).enumerate() {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Turn a hex-encoded Ed25519 public key into an anchor [`verify`] can use.
///
/// Public so a test elsewhere in the crate can build a real anchor from
/// [`generate_issuing_keypair`] and exercise genuine `Active`/`Expired`
/// statuses, instead of asserting against hand-built enum values that could
/// drift away from what verification actually produces.
pub fn issuing_key_from_hex(hex: &str) -> Result<VerifyingKey, String> {
    let raw =
        from_hex32(hex).ok_or_else(|| "an issuing key must be 32 hex-encoded bytes".to_string())?;
    VerifyingKey::from_bytes(&raw).map_err(|e| format!("not a valid Ed25519 public key: {e}"))
}

/// The compiled-in trust anchor, or why there isn't one.
pub fn issuing_key() -> Result<VerifyingKey, String> {
    if ISSUING_PUBKEY_HEX == UNPROVISIONED {
        return Err(
            "this build has no license issuing key compiled in, so no key can be verified against it \
             (it was built without DRUMS_LICENSE_PUBKEY)"
                .to_string(),
        );
    }
    issuing_key_from_hex(ISSUING_PUBKEY_HEX)
        .map_err(|e| format!("the license issuing key compiled into this build is unusable: {e}"))
}

/// Verify one key string against one anchor at one instant.
///
/// Pure — `now_ms` is data, matching [`crate::digest::Digest::build`]'s own
/// discipline, so expiry behaviour is testable without touching the clock.
///
/// The signature is checked over the base64-DECODED payload bytes, before
/// they are parsed as JSON. That ordering is load-bearing twice over: no
/// attacker-controlled JSON is ever deserialized until the signature has
/// already passed, and there is no canonicalization problem to get wrong,
/// because the bytes signed are literally the bytes carried.
///
/// `verify_strict` rather than `verify`: it additionally rejects weak
/// (low-order) public keys, which is free here and closes the weak-key
/// forgery family outright.
pub fn verify(key: &str, anchor: &VerifyingKey, now_ms: u64) -> LicenseStatus {
    let invalid = |why: &str| LicenseStatus::Invalid {
        why: why.to_string(),
    };

    let Some(rest) = key.strip_prefix(KEY_PREFIX) else {
        return invalid(&format!(
            "it does not start with `{KEY_PREFIX}`, so it is not a Drums license key"
        ));
    };
    let Some((payload_b64, sig_b64)) = rest.split_once('.') else {
        return invalid(
            "it is missing the `.` between its payload and its signature — it looks truncated",
        );
    };
    let Ok(payload) = b64().decode(payload_b64) else {
        return invalid("its payload is not valid base64url — it looks corrupted, or was line-wrapped when it was copied");
    };
    let Ok(sig_bytes) = b64().decode(sig_b64) else {
        return invalid("its signature is not valid base64url — it looks corrupted, or was line-wrapped when it was copied");
    };
    let Ok(sig_bytes): Result<[u8; 64], _> = sig_bytes.try_into() else {
        return invalid("its signature is not 64 bytes long");
    };
    let sig = Signature::from_bytes(&sig_bytes);
    if anchor.verify_strict(&payload, &sig).is_err() {
        return invalid(
            "its signature does not match the issuing key this build trusts — it was not issued by Drums, \
             or it was edited after it was issued",
        );
    }

    // Only now, with the bytes proven, is any of this parsed.
    let Ok(license) = serde_json::from_slice::<License>(&payload) else {
        return invalid("its payload is signed but is not a license this version understands");
    };
    if license.v != 1 {
        return invalid(&format!(
            "it declares payload version {}, and this build understands version 1",
            license.v
        ));
    }
    if now_ms > license.expires_ms {
        return LicenseStatus::Expired(license);
    }
    LicenseStatus::Active(license)
}

/// [`verify`] against the compiled-in anchor, tolerating "no key at all".
pub fn status_from(key: Option<&str>, now_ms: u64) -> LicenseStatus {
    let Some(key) = key else {
        return LicenseStatus::Absent;
    };
    let key = key.trim();
    if key.is_empty() {
        return LicenseStatus::Absent;
    }
    match issuing_key() {
        Ok(anchor) => verify(key, &anchor, now_ms),
        Err(why) => LicenseStatus::Invalid { why },
    }
}

// -- where a key lives -------------------------------------------------------

/// The file `drums activate` writes.
///
/// Per MACHINE, not per repo: a license covers a customer, and putting it in
/// `.drums/` would mean committing it or re-activating in every checkout.
/// `DRUMS_LICENSE_FILE` overrides it (tests, CI images, and anyone who keeps
/// their config somewhere unusual).
pub fn license_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("DRUMS_LICENSE_FILE") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return Some(PathBuf::from(x).join("drums").join("license.key"));
        }
    }
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty())?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("drums")
            .join("license.key"),
    )
}

/// The key this machine would use, if any. `DRUMS_LICENSE` wins over the
/// file so a container image can carry one without a writable home.
fn stored_key() -> Option<String> {
    if let Ok(k) = std::env::var("DRUMS_LICENSE") {
        if !k.trim().is_empty() {
            return Some(k);
        }
    }
    let path = license_path()?;
    let body = std::fs::read_to_string(path).ok()?;
    if body.trim().is_empty() {
        return None;
    }
    Some(body)
}

/// What this machine's license is, right now. The only function here that
/// reads ambient state; everything that makes a DECISION takes the resulting
/// [`LicenseStatus`] as an argument.
pub fn current_status(now_ms: u64) -> LicenseStatus {
    status_from(stored_key().as_deref(), now_ms)
}

/// `drums activate <key>`: verify first, store only if it is genuinely
/// usable.
///
/// Storing an unverifiable key would turn one clear failure at activation
/// into a confusing one later, at the moment the customer actually wanted to
/// promote a class.
pub fn activate(key: &str, now_ms: u64) -> Result<(License, PathBuf), String> {
    let status = status_from(Some(key), now_ms);
    let license = match status {
        LicenseStatus::Active(l) => l,
        LicenseStatus::Expired(l) => {
            return Err(format!(
                "this key expired {} (issued to {}) — an expired key grants nothing, so it was not stored",
                relative_when(l.expires_ms, now_ms),
                l.customer
            ))
        }
        LicenseStatus::Invalid { why } => return Err(format!("this key cannot be used: {why}")),
        LicenseStatus::Absent => return Err("no key was given".to_string()),
    };

    let path = license_path().ok_or_else(|| {
        "could not work out where to store the key: neither DRUMS_LICENSE_FILE, XDG_CONFIG_HOME nor HOME is set"
            .to_string()
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    std::fs::write(&path, format!("{}\n", key.trim()))
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok((license, path))
}

// -- what a refusal says -----------------------------------------------------

/// How long until `future_ms`, in the same register as [`relative_when`].
fn relative_until(future_ms: u64, now_ms: u64) -> String {
    let secs = future_ms.saturating_sub(now_ms) / 1000;
    if secs < 60 {
        "in under a minute".to_string()
    } else if secs < 3_600 {
        format!("in {}m", secs / 60)
    } else if secs < 86_400 {
        format!("in {}h", secs / 3_600)
    } else {
        format!("in {}d", secs / 86_400)
    }
}

/// One sentence about the key on this machine, for the refusal.
fn status_sentence(status: &LicenseStatus, now_ms: u64) -> String {
    match status {
        LicenseStatus::Absent => "There is no license key on this machine.".to_string(),
        LicenseStatus::Expired(l) => format!(
            "The key on this machine (issued to {}) expired {} — an expired key grants nothing.",
            l.customer,
            relative_when(l.expires_ms, now_ms)
        ),
        LicenseStatus::Invalid { why } => format!("The key on this machine cannot be used: {why}."),
        // Unreachable from the gate, which only refuses when act-alone is
        // NOT granted. Stated rather than `unreachable!` so a future caller
        // gets a sentence instead of a panic.
        LicenseStatus::Active(_) => "The key on this machine is active.".to_string(),
    }
}

/// The refusal `drums authority promote` prints for a class that has earned
/// act-alone on a machine with no valid license.
///
/// Written to be read once by someone who just did everything right. It
/// states the boundary, credits the evidence they actually earned by name,
/// spells out that the rest of the product is unaffected, and stops. No
/// price (that is a conversation — `docs/PRICING.md`), no countdown, no
/// second ask.
pub fn act_alone_refusal(class: &str, streak: u32, status: &LicenseStatus, now_ms: u64) -> String {
    format!(
        "drums authority promote: act-alone is the paid part of Drums.\n\
         \n\
         \x20 {class} has earned it — {streak} consecutive repairs shipped and stayed shipped.\n\
         \x20 That evidence is in your own record and it keeps accruing either way.\n\
         \n\
         \x20 Free, forever, with no key and no account: detect, attribute, reproduce, repair,\n\
         \x20 verify, propose. Everything up to and including a verified repair waiting for you\n\
         \x20 at `drums ship` works exactly as it does today. This command is the only thing\n\
         \x20 that is gated, and nothing you already have stops working.\n\
         \n\
         \x20 Paid: letting Drums ship this class without waiting for you.\n\
         \n\
         \x20 {}\n\
         \n\
         \x20 To talk about it: {TALK_URL}\n\
         \x20 If you already have a key: drums activate <key>\n",
        status_sentence(status, now_ms)
    )
}

/// The one extra line the morning message adds under an earned promotion
/// when act-alone is not licensed. Deliberately a single sentence: the
/// evidence is the pitch, and a digest that argues is a digest people mute.
pub fn digest_paid_note() -> String {
    format!("Act-alone is the paid part of Drums; everything you are running today stays free. {TALK_URL}")
}

/// What `drums activate` prints on success.
pub fn activated_message(license: &License, path: &std::path::Path, now_ms: u64) -> String {
    format!(
        "license active — issued to {}, {} tier, expires {}.\n\
         stored at {}\n\
         act-alone can now be granted per class with `drums authority promote <class>`. Each class still\n\
         has to earn it, and one rollback still drops it straight back to propose.\n",
        license.customer,
        license.tier,
        relative_until(license.expires_ms, now_ms),
        path.display()
    )
}

// -- issuing (founder-side; the private key never touches this repo) ---------

/// Mint a fresh issuing keypair: `(private_hex, public_hex)`.
///
/// Run ONCE, on a machine the founder trusts. The public half goes into
/// [`ISSUING_PUBKEY_HEX`] (or `DRUMS_LICENSE_PUBKEY` at build time); the
/// private half goes wherever secrets go and is never committed, never put
/// in CI, and never shipped. Losing it means every outstanding key has to be
/// reissued; leaking it means anyone can mint.
pub fn generate_issuing_keypair() -> Result<(String, String), String> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| format!("could not read OS randomness: {e}"))?;
    let signing = SigningKey::from_bytes(&seed);
    let public = to_hex(signing.verifying_key().as_bytes());
    let private = to_hex(&signing.to_bytes());
    Ok((private, public))
}

/// Issue one key. `signing_key_hex` is the private half from
/// [`generate_issuing_keypair`], supplied by the founder at mint time and
/// never read from anywhere in this repo.
pub fn mint(signing_key_hex: &str, license: &License) -> Result<String, String> {
    let seed = from_hex32(signing_key_hex)
        .ok_or_else(|| "the signing key must be 32 hex-encoded bytes".to_string())?;
    let signing = SigningKey::from_bytes(&seed);
    // Serialize ONCE and sign those exact bytes — the same bytes the key
    // carries and `verify` checks. Re-serializing at verification time would
    // make the signature depend on field ordering, which is a bug waiting
    // for the first time someone reorders the struct.
    let payload = serde_json::to_vec(license)
        .map_err(|e| format!("could not serialize the license payload: {e}"))?;
    let sig = signing.sign(&payload);
    Ok(format!(
        "{KEY_PREFIX}{}.{}",
        b64().encode(&payload),
        b64().encode(sig.to_bytes())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400_000;
    const NOW: u64 = 1_800_000_000_000;

    fn a_license() -> License {
        License {
            v: 1,
            customer: "Caelon Systems".to_string(),
            tier: "team".to_string(),
            issued_ms: NOW - 30 * DAY,
            expires_ms: NOW + 335 * DAY,
        }
    }

    /// An ephemeral keypair, generated here and thrown away when the test
    /// ends. This is the ONLY place a private key exists anywhere in this
    /// repository, and it does not outlive the process.
    fn ephemeral() -> (String, VerifyingKey) {
        let (private_hex, public_hex) = generate_issuing_keypair().unwrap();
        (private_hex, issuing_key_from_hex(&public_hex).unwrap())
    }

    /// **This is the minting recipe.** Three lines, run once per customer on
    /// the founder's own machine with the private half of the issuing
    /// keypair — never in CI, never from a checkout of this repo.
    ///
    /// ```text
    /// let (private_hex, public_hex) = license::generate_issuing_keypair()?;  // ONCE, ever
    /// //   -> paste public_hex into ISSUING_PUBKEY_HEX (or build with DRUMS_LICENSE_PUBKEY=<public_hex>)
    /// //   -> keep private_hex out of the repo, out of CI, out of the binary
    /// let key = license::mint(&private_hex, &License { v: 1, customer: "...".into(),
    ///                                                  tier: "team".into(),
    ///                                                  issued_ms: now, expires_ms: now + a_year })?;
    /// //   -> send `key` to the customer; they run `drums activate <key>`
    /// ```
    #[test]
    fn a_minted_key_verifies_and_the_private_half_never_leaves_this_test() {
        let (private_hex, anchor) = ephemeral();
        let key = mint(&private_hex, &a_license()).unwrap();

        assert!(key.starts_with(KEY_PREFIX), "{key}");
        assert_eq!(
            verify(&key, &anchor, NOW),
            LicenseStatus::Active(a_license())
        );
        assert!(verify(&key, &anchor, NOW).grants_act_alone());
        // The key a customer receives carries the signed payload and nothing
        // else — no secret material travels with it.
        assert!(
            !key.contains(&private_hex),
            "the private key must never appear in an issued key"
        );
    }

    #[test]
    fn a_key_from_a_different_issuer_is_refused_not_trusted() {
        let (attacker_private, _) = ephemeral();
        let (_, real_anchor) = ephemeral();
        let key = mint(&attacker_private, &a_license()).unwrap();
        let status = verify(&key, &real_anchor, NOW);
        assert!(
            matches!(status, LicenseStatus::Invalid { .. }),
            "{status:?}"
        );
        assert!(!status.grants_act_alone());
        let LicenseStatus::Invalid { why } = status else {
            unreachable!()
        };
        assert!(why.contains("does not match the issuing key"), "{why}");
    }

    /// The obvious attack: edit the expiry (or the customer) in the payload
    /// and re-encode. The signature covers the exact bytes, so it fails.
    #[test]
    fn a_tampered_payload_fails_closed_and_says_the_signature_did_not_match() {
        let (private_hex, anchor) = ephemeral();
        let key = mint(&private_hex, &a_license()).unwrap();

        let rest = key.strip_prefix(KEY_PREFIX).unwrap();
        let (payload_b64, sig_b64) = rest.split_once('.').unwrap();
        let mut payload: License =
            serde_json::from_slice(&b64().decode(payload_b64).unwrap()).unwrap();
        payload.expires_ms += 100 * 365 * DAY;
        let forged = format!(
            "{KEY_PREFIX}{}.{sig_b64}",
            b64().encode(serde_json::to_vec(&payload).unwrap())
        );

        let status = verify(&forged, &anchor, NOW);
        assert!(!status.grants_act_alone(), "{status:?}");
        let LicenseStatus::Invalid { why } = status else {
            panic!("{status:?}")
        };
        assert!(why.contains("signature does not match"), "{why}");
    }

    #[test]
    fn an_expired_key_grants_nothing_and_says_it_expired() {
        let (private_hex, anchor) = ephemeral();
        let expired = License {
            expires_ms: NOW - 3 * DAY,
            ..a_license()
        };
        let key = mint(&private_hex, &expired).unwrap();

        let status = verify(&key, &anchor, NOW);
        assert_eq!(status, LicenseStatus::Expired(expired.clone()));
        assert!(
            !status.grants_act_alone(),
            "an expired key must fail closed"
        );

        // ...and the refusal it produces names the expiry, not a shrug.
        let msg = act_alone_refusal("shop/TypeError", 5, &status, NOW);
        assert!(msg.contains("expired 3d ago"), "{msg}");
        assert!(msg.contains("Caelon Systems"), "{msg}");
    }

    /// The boundary itself: one millisecond either side.
    #[test]
    fn expiry_is_checked_against_the_instant_passed_in_not_the_wall_clock() {
        let (private_hex, anchor) = ephemeral();
        let l = License {
            expires_ms: NOW,
            ..a_license()
        };
        let key = mint(&private_hex, &l).unwrap();
        assert!(
            verify(&key, &anchor, NOW).grants_act_alone(),
            "expiring exactly now is still valid"
        );
        assert!(
            !verify(&key, &anchor, NOW + 1).grants_act_alone(),
            "one ms past expiry is not"
        );
    }

    #[test]
    fn every_malformed_shape_is_refused_with_a_reason_that_names_the_problem() {
        let (private_hex, anchor) = ephemeral();
        let good = mint(&private_hex, &a_license()).unwrap();
        let rest = good.strip_prefix(KEY_PREFIX).unwrap();
        let (payload_b64, sig_b64) = rest.split_once('.').unwrap();

        let cases: Vec<(String, &str)> = vec![
            ("hunter2".to_string(), "not a Drums license key"),
            (format!("{KEY_PREFIX}{payload_b64}"), "missing the `.`"),
            (
                format!("{KEY_PREFIX}not base64!!.{sig_b64}"),
                "payload is not valid base64url",
            ),
            (
                format!("{KEY_PREFIX}{payload_b64}.not base64!!"),
                "signature is not valid base64url",
            ),
            (
                format!("{KEY_PREFIX}{payload_b64}.{}", b64().encode([0u8; 8])),
                "signature is not 64 bytes",
            ),
        ];
        for (key, expected) in cases {
            let status = verify(&key, &anchor, NOW);
            assert!(!status.grants_act_alone(), "{key} must not grant act-alone");
            let LicenseStatus::Invalid { why } = status else {
                panic!("expected Invalid for {key}")
            };
            assert!(
                why.contains(expected),
                "for {key:?}: expected {expected:?} in {why:?}"
            );
        }
    }

    /// A signed payload from a scheme this build does not understand must be
    /// refused, not half-read. Signed with the RIGHT key, so only the
    /// version check can catch it.
    #[test]
    fn a_signed_payload_from_a_future_version_is_refused_rather_than_guessed_at() {
        let (private_hex, anchor) = ephemeral();
        let key = mint(
            &private_hex,
            &License {
                v: 2,
                ..a_license()
            },
        )
        .unwrap();
        let status = verify(&key, &anchor, NOW);
        let LicenseStatus::Invalid { why } = status else {
            panic!("{status:?}")
        };
        assert!(why.contains("version 2"), "{why}");
    }

    #[test]
    fn no_key_at_all_is_absent_which_is_not_an_error() {
        assert_eq!(status_from(None, NOW), LicenseStatus::Absent);
        assert_eq!(status_from(Some("   "), NOW), LicenseStatus::Absent);
        assert!(!LicenseStatus::Absent.grants_act_alone());
    }

    /// A build with no anchor cannot verify anything, and says so precisely
    /// rather than pretending the customer's key is bad.
    #[test]
    fn a_build_with_no_issuing_key_says_that_is_the_problem() {
        if ISSUING_PUBKEY_HEX != UNPROVISIONED {
            return; // a provisioned release build; nothing to assert here
        }
        let status = status_from(Some("drums-license-v1.aaa.bbb"), NOW);
        let LicenseStatus::Invalid { why } = status else {
            panic!("{status:?}")
        };
        assert!(why.contains("no license issuing key compiled in"), "{why}");
    }

    // -- the refusal a paying-capable customer actually reads ---------------

    #[test]
    fn the_refusal_credits_the_evidence_names_what_stays_free_and_points_somewhere_real() {
        let msg = act_alone_refusal("shop/TypeError", 5, &LicenseStatus::Absent, NOW);

        // States the boundary plainly.
        assert!(msg.contains("act-alone is the paid part of Drums"), "{msg}");
        // Names the exact evidence already earned.
        assert!(
            msg.contains("shop/TypeError has earned it — 5 consecutive repairs"),
            "{msg}"
        );
        // Says what is kept for free, by name — the whole loop up to propose.
        for kept in [
            "detect",
            "attribute",
            "reproduce",
            "repair",
            "verify",
            "propose",
        ] {
            assert!(
                msg.contains(kept),
                "the free path must be named in full, missing {kept}: {msg}"
            );
        }
        assert!(
            msg.contains("nothing you already have stops working"),
            "{msg}"
        );
        // Points at a page that exists, and quotes no price.
        assert!(msg.contains("https://drums.sh/#pricing"), "{msg}");
        assert!(!msg.contains('$'), "no price may be invented here: {msg}");
        assert!(msg.contains("drums activate <key>"), "{msg}");
    }

    /// The tone gate. This message is read by someone who just did
    /// everything right, on the day they earned something.
    #[test]
    fn the_refusal_is_not_sneering_and_does_not_nag() {
        let msg =
            act_alone_refusal("shop/TypeError", 7, &LicenseStatus::Absent, NOW).to_lowercase();
        for banned in [
            "upgrade now",
            "unlock",
            "premium",
            "pro plan",
            "sorry",
            "unfortunately",
            "!",
            "trial",
            "limited time",
            "just ",
            "simply ",
            "you'll need to",
            "hurry",
        ] {
            assert!(
                !msg.contains(banned),
                "{banned:?} has no place in this message: {msg}"
            );
        }
        // Asked once. The word "paid" appears exactly twice — the statement
        // and the contrast with "Free" — and never a third time.
        assert_eq!(msg.matches("paid").count(), 2, "{msg}");
    }

    #[test]
    fn the_digest_note_is_one_sentence_and_says_what_stays_free() {
        let note = digest_paid_note();
        assert!(note.contains("stays free"), "{note}");
        assert!(note.contains(TALK_URL), "{note}");
        assert_eq!(
            note.matches('.').count(),
            2,
            "one sentence plus the URL's own dots: {note}"
        );
    }

    // -- storage ------------------------------------------------------------

    #[test]
    fn activate_refuses_to_store_a_key_it_could_not_verify() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("license.key");
        // Explicit path, so this test never touches the developer's real
        // ~/.config/drums/license.key.
        let err = activate_at(&path, "drums-license-v1.garbage.garbage", NOW).unwrap_err();
        assert!(err.contains("cannot be used"), "{err}");
        assert!(
            !path.exists(),
            "an unverifiable key must not be written to disk"
        );
    }

    /// `activate` with an explicit destination — the same body as
    /// [`activate`] minus `license_path()`, so the storage half is testable
    /// without setting a process-wide environment variable.
    fn activate_at(path: &std::path::Path, key: &str, now_ms: u64) -> Result<License, String> {
        let status = status_from(Some(key), now_ms);
        match status {
            LicenseStatus::Active(l) => {
                std::fs::write(path, key).map_err(|e| e.to_string())?;
                Ok(l)
            }
            LicenseStatus::Expired(l) => {
                Err(format!("this key expired (issued to {})", l.customer))
            }
            LicenseStatus::Invalid { why } => Err(format!("this key cannot be used: {why}")),
            LicenseStatus::Absent => Err("no key was given".to_string()),
        }
    }

    #[test]
    fn hex_decoding_refuses_anything_that_is_not_exactly_thirty_two_bytes() {
        assert!(from_hex32("").is_none());
        assert!(from_hex32(&"a".repeat(63)).is_none());
        assert!(from_hex32(&"a".repeat(65)).is_none());
        assert!(from_hex32(&"z".repeat(64)).is_none());
        assert_eq!(from_hex32(UNPROVISIONED), Some([0u8; 32]));
        // Round trips.
        let (private_hex, _) = ephemeral();
        assert_eq!(to_hex(&from_hex32(&private_hex).unwrap()), private_hex);
    }

    #[test]
    fn relative_until_reads_the_way_a_person_would_say_it() {
        assert_eq!(relative_until(NOW + 30_000, NOW), "in under a minute");
        assert_eq!(relative_until(NOW + 5 * 60_000, NOW), "in 5m");
        assert_eq!(relative_until(NOW + 5 * 3_600_000, NOW), "in 5h");
        assert_eq!(relative_until(NOW + 365 * DAY, NOW), "in 365d");
        assert_eq!(
            relative_until(NOW - DAY, NOW),
            "in under a minute",
            "saturates rather than underflowing"
        );
    }
}
