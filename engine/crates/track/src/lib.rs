//! The issue-tracker seam (`docs/CONTRACTS.md`): writing the evidence back to
//! where a human reported the problem.
//!
//! ## Why this is a separate seam from `engine-propose`
//!
//! A pull request is where a *change* is reviewed. An issue is where a
//! *complaint* lives. They are different audiences: the reviewer wants the
//! diff and the claims; the person who filed the issue wants to know whether
//! anyone did anything, and they will never open a PR to find out. Closing the
//! loop means answering on the thread they started.
//!
//! ## Why an API key here, when `gh` needed none
//!
//! `engine-propose` drives `gh` because a CLI carrying the team's own login
//! already exists. Linear has no such CLI, so this takes an API key —
//! **their** key for **their** tracker. That is a different thing from an
//! Anthropic or OpenAI key, and the distinction is the whole
//! agent-neutrality principle: Drums never asks for a key that buys model
//! tokens, because Drums never pays for inference. A tracker key buys
//! nothing; it is an authorization to write to a system the customer already
//! owns.
//!
//! The key is read from the environment and never persisted, never logged,
//! and never written to the record.
//!
//! ## What a comment is allowed to say
//!
//! Exactly what the claims say, with the chips intact — and nothing about
//! visual or UX correctness, which no check in this product can establish.
//! A reported issue is very often a *visual* complaint ("the button overlaps
//! the price"), and the single most dishonest thing this code could do is
//! reply "fixed" to one. So the comment states what was executed, and states
//! plainly that whether it *looks* right is unresolved and needs a human.

use async_trait::async_trait;
use engine_core::{Claim, Provenance};
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum TrackError {
    #[error("no {tracker} credential configured ({env_var} is unset)")]
    NoCredential { tracker: &'static str, env_var: &'static str },
    #[error("{tracker} rejected the request: {detail}")]
    Rejected { tracker: &'static str, detail: String },
    #[error("could not reach {tracker}: {detail}")]
    Unreachable { tracker: &'static str, detail: String },
    #[error("this issue reference has no id to comment on")]
    NoIssueId,
}

/// Which issue to write to. Deliberately minimal: an implementation gets the
/// tracker's own id and nothing else, so nothing about our internal model
/// leaks into a third-party system.
#[derive(Debug, Clone)]
pub struct IssueRef {
    /// The tracker's own identifier (Linear's UUID, not `DRM-42`).
    pub id: String,
    /// Which tracker this id belongs to — `linear`, `agentation`.
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct Comment {
    pub url: Option<String>,
    /// One claim: that a comment was posted. `Observed` — the code saw the
    /// tracker accept it. Never `Verified`: posting a comment is an action,
    /// not a check whose outcome was measured.
    pub claim: Claim,
}

#[async_trait]
pub trait IssueTracker: Send + Sync {
    fn name(&self) -> &'static str;

    /// Post `body` as a comment on `issue`. Must be safe to call twice: a
    /// duplicate comment is noise, but a lost one means the person who
    /// reported the problem never hears back, which is worse.
    async fn comment(&self, issue: &IssueRef, body: &str) -> Result<Comment, TrackError>;
}

// -- comment rendering -------------------------------------------------------

/// The comment body. Same evidence discipline as the PR body, in the register
/// of a reply rather than a proposal.
///
/// `proposal_url` is `None` when no proposal was opened — in which case the
/// comment says what happened and stops, rather than implying there is
/// somewhere to go and look.
pub fn render_comment(
    claims: &[Claim],
    proposal_url: Option<&str>,
    repaired: bool,
) -> String {
    let mut s = String::new();

    if repaired {
        s.push_str("**Drums has a proposed fix for this.**\n\n");
    } else {
        s.push_str("**Drums looked at this and could not produce a verified fix.**\n\n");
    }

    s.push_str("What it checked by running it:\n\n");
    let verified: Vec<&Claim> = claims
        .iter()
        .filter(|c| c.provenance == Provenance::Verified)
        .collect();
    if verified.is_empty() {
        s.push_str("- nothing. No check completed, so nothing here is verified.\n");
    } else {
        for c in &verified {
            s.push_str(&format!("- `verified` — {}\n", c.text));
        }
    }
    s.push('\n');

    let unresolved: Vec<&Claim> = claims
        .iter()
        .filter(|c| c.provenance == Provenance::Unresolved)
        .collect();

    // Always present, even with zero unresolved claims. A reported issue is
    // usually a visual complaint, and replying "fixed" to one on the strength
    // of a passing test suite is the most dishonest thing this code could do.
    s.push_str("What it could **not** check:\n\n");
    s.push_str(
        "- `unresolved` — whether this now *looks* right. Drums has no visual check, \
         so a human has to confirm the appearance.\n",
    );
    for c in &unresolved {
        s.push_str(&format!("- `unresolved` — {}\n", c.text));
    }
    s.push('\n');

    match proposal_url {
        Some(url) => s.push_str(&format!("Review the change: {url}\n\n")),
        None => s.push_str("No pull request was opened for this.\n\n"),
    }

    s.push_str(
        "_Nothing was merged or deployed automatically. `verified` means Drums ran a \
         check and observed the result; `unresolved` means it could not tell — never \
         that it is fine._\n",
    );
    s
}

// -- Linear ------------------------------------------------------------------

const LINEAR_ENV: &str = "DRUMS_LINEAR_API_KEY";

/// Comments on Linear issues over its GraphQL API.
#[derive(Debug, Clone)]
pub struct LinearTracker {
    api_key: String,
    /// Overridable so tests can point at a local server. Production callers
    /// use [`LinearTracker::from_env`].
    endpoint: String,
}

impl LinearTracker {
    pub const DEFAULT_ENDPOINT: &'static str = "https://api.linear.app/graphql";

    /// `None` when no key is configured — which is a normal, silent state, not
    /// an error. A team that hasn't wired Linear should see nothing about
    /// Linear.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var(LINEAR_ENV).ok().filter(|k| !k.trim().is_empty())?;
        Some(Self { api_key, endpoint: Self::DEFAULT_ENDPOINT.to_string() })
    }

    pub fn with_endpoint(api_key: impl Into<String>, endpoint: impl Into<String>) -> Self {
        Self { api_key: api_key.into(), endpoint: endpoint.into() }
    }
}

#[derive(Deserialize)]
struct GqlResponse {
    #[serde(default)]
    data: Option<GqlData>,
    #[serde(default)]
    errors: Option<Vec<GqlError>>,
}

#[derive(Deserialize)]
struct GqlData {
    #[serde(rename = "commentCreate")]
    comment_create: Option<CommentCreate>,
}

#[derive(Deserialize)]
struct CommentCreate {
    success: bool,
    #[serde(default)]
    comment: Option<CommentNode>,
}

#[derive(Deserialize)]
struct CommentNode {
    #[serde(default)]
    url: Option<String>,
}

#[derive(Deserialize)]
struct GqlError {
    message: String,
}

#[async_trait]
impl IssueTracker for LinearTracker {
    fn name(&self) -> &'static str {
        "linear"
    }

    async fn comment(&self, issue: &IssueRef, body: &str) -> Result<Comment, TrackError> {
        if issue.id.trim().is_empty() {
            return Err(TrackError::NoIssueId);
        }

        // Variables, not string interpolation. The body contains production
        // error text and an agent-written summary; splicing either into a
        // GraphQL document would be the same class of mistake as building a
        // shell command out of them.
        let payload = serde_json::json!({
            "query": "mutation Comment($issueId: String!, $body: String!) { \
                        commentCreate(input: { issueId: $issueId, body: $body }) { \
                          success comment { url } } }",
            "variables": { "issueId": issue.id, "body": body },
        });

        let client = reqwest::Client::new();
        let res = client
            .post(&self.endpoint)
            // Linear personal API keys go in Authorization with NO "Bearer "
            // prefix; OAuth tokens use Bearer. Sending the wrong shape fails
            // with a generic 400, so this is worth stating rather than
            // rediscovering.
            .header("Authorization", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| TrackError::Unreachable { tracker: "linear", detail: e.to_string() })?;

        let status = res.status();
        let text = res.text().await.unwrap_or_default();

        if !status.is_success() {
            return Err(TrackError::Rejected {
                tracker: "linear",
                // Never echo the response wholesale: a tracker's error body
                // can contain the request it is complaining about, and this
                // string reaches a terminal.
                detail: format!("HTTP {}", status.as_u16()),
            });
        }

        // GraphQL answers 200 with an `errors` array, so status alone proves
        // nothing.
        let parsed: GqlResponse = serde_json::from_str(&text).map_err(|_| TrackError::Rejected {
            tracker: "linear",
            detail: "response was not the expected GraphQL shape".to_string(),
        })?;

        if let Some(errs) = parsed.errors.as_ref().filter(|e| !e.is_empty()) {
            let first = errs[0].message.chars().take(200).collect::<String>();
            return Err(TrackError::Rejected { tracker: "linear", detail: first });
        }

        let created = parsed
            .data
            .and_then(|d| d.comment_create)
            .filter(|c| c.success)
            .ok_or_else(|| TrackError::Rejected {
                tracker: "linear",
                detail: "commentCreate did not report success".to_string(),
            })?;

        let url = created.comment.and_then(|c| c.url);
        Ok(Comment {
            claim: Claim {
                text: match &url {
                    Some(u) => format!("commented on the linear issue at {u}"),
                    None => "commented on the linear issue".to_string(),
                },
                provenance: Provenance::Observed,
            },
            url,
        })
    }
}

/// Build a tracker for a reported-issue source, if one is configured.
/// `agentation` has no write API wired, so it returns `None` rather than
/// pretending a comment went somewhere.
pub fn for_source(source: &str) -> Option<Box<dyn IssueTracker>> {
    match source {
        "linear" => LinearTracker::from_env().map(|t| Box::new(t) as Box<dyn IssueTracker>),
        _ => None,
    }
}
