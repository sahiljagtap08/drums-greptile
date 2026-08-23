//! Building the behavior source the loop reads through — PostHog today.
//!
//! Two ways in, tried in this order:
//!
//! 1. **Direct** — host and project from `.drums/config.toml` (`posthog_host`,
//!    `posthog_project`) or `DRUMS_POSTHOG_HOST` / `DRUMS_POSTHOG_PROJECT`
//!    (env wins), key from `DRUMS_POSTHOG_API_KEY` ONLY — never the file,
//!    because a secret in a committed file is a leak with a delay. This is
//!    local mode's path: no account required, nothing leaves the machine but
//!    the queries themselves.
//! 2. **The plane bridge** — a machine that has run `drums login` sends its
//!    queries to the console, which spends the OAuth grant the account
//!    already made in Integrations. No analytics credential on this machine
//!    at all. An explicitly configured key beats the bridge because a person
//!    who set three variables by hand meant it.
//!
//! The stored credential must match the console this run would talk to —
//! the same staging-token guard `dispatch::RemoteRepairs::from_login`
//! enforces, for the same reason: a mismatch should read as a sentence, not
//! as a confusing refusal from the wrong deployment.

use std::sync::Arc;

use engine_behavior::{BehaviorSource, Plane, PostHog};

use crate::config::Config;

/// What is missing, named, for refusals and for doctor. Empty means a source
/// can be built.
pub fn missing(config: &Config) -> Vec<&'static str> {
    if bridge_available() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if std::env::var("DRUMS_POSTHOG_HOST").is_err() && config.posthog_host.is_none() {
        out.push("posthog_host (config) or DRUMS_POSTHOG_HOST (env)");
    }
    if std::env::var("DRUMS_POSTHOG_PROJECT").is_err() && config.posthog_project.is_none() {
        out.push("posthog_project (config) or DRUMS_POSTHOG_PROJECT (env)");
    }
    if std::env::var("DRUMS_POSTHOG_API_KEY").is_err() {
        out.push("DRUMS_POSTHOG_API_KEY (env only — never the config file)");
    }
    if !out.is_empty() {
        out.push("or instead of all three: drums login, and connect PostHog in the console");
    }
    out
}

/// The source, when everything it needs is present. `None` is a normal state —
/// the error-path loop runs without it — and every caller that NEEDS it says
/// what is missing by name via [`missing`].
pub fn build(config: &Config) -> Option<Arc<dyn BehaviorSource>> {
    if let Some(direct) = direct(config) {
        return Some(direct);
    }
    bridge()
}

fn direct(config: &Config) -> Option<Arc<dyn BehaviorSource>> {
    let host = std::env::var("DRUMS_POSTHOG_HOST")
        .ok()
        .or_else(|| config.posthog_host.clone())?;
    let project = std::env::var("DRUMS_POSTHOG_PROJECT")
        .ok()
        .or_else(|| config.posthog_project.clone())?;
    let key = std::env::var("DRUMS_POSTHOG_API_KEY").ok()?;
    Some(Arc::new(PostHog::new(host, project, key)))
}

fn bridge() -> Option<Arc<dyn BehaviorSource>> {
    let creds = crate::login::load().ok().flatten()?;
    let console = crate::login::console_url();
    if !creds.console_url.is_empty() && creds.console_url != console {
        return None;
    }
    Some(Arc::new(Plane::new(console, creds.token)))
}

fn bridge_available() -> bool {
    crate::login::load()
        .ok()
        .flatten()
        .map(|c| c.console_url.is_empty() || c.console_url == crate::login::console_url())
        .unwrap_or(false)
}
