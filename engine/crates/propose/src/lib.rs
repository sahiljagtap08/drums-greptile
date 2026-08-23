//! The **output** seam (`docs/CONTRACTS.md`): turning a verified repair into
//! something a team reviews where they already review things.
//!
//! ## Why this exists
//!
//! Everything upstream of here — detect, attribute, reproduce, repair, verify
//! — produces a branch and a set of claims. That is the whole product, and it
//! is invisible: nobody watches a terminal at 03:00. The proposal is where
//! the work becomes something a human actually encounters, and the evidence
//! travels *with* it. A pull request whose body is the claims table is the
//! difference between "a bot pushed a branch" and "here is what broke, here
//! is proof this fixes it, here is what I could not check".
//!
//! ## Why `gh` and not the GitHub API
//!
//! Same reason `engine-repair` drives `claude`/`codex` instead of calling
//! model APIs: the team is already authenticated. `gh` carries their login,
//! their SSO, their org policy, their audit trail. Drums asks for no token,
//! stores no credential, and adds no OAuth app to anyone's security review.
//! When they revoke that login, Drums loses access — which is the correct
//! behaviour and costs us nothing to implement.
//!
//! Consequently a "GitHub adapter" here is a *command*, not an API client
//! (the same ruling `docs/CONTRACTS.md` makes about deployment).
//!
//! ## Honesty rules this module enforces
//!
//! 1. **A proposal never invents a claim.** The body restates the claims the
//!    pipeline already earned, chip for chip. This module contributes exactly
//!    one claim of its own — that a proposal was opened, at a URL it read
//!    back from the command's own output.
//! 2. **Unresolved is never hidden.** If any claim is `unresolved`, the body
//!    says so above the fold, in its own section, before the diff.
//! 3. **argv only, never a shell.** Every value below is attacker-influenced
//!    somewhere upstream (a branch name derived from a failure id, a summary
//!    written by an agent, an error message from production). Nothing here is
//!    ever interpolated into a shell string.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use engine_core::{Attribution, Claim, Failure, Provenance, Repair, Reproduction};

/// How long any single `git`/`gh` invocation may take before it is killed.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, thiserror::Error)]
pub enum ProposalError {
    #[error("`{tool}` is not installed or not on PATH")]
    ToolMissing { tool: &'static str },
    #[error("`{tool}` is installed but not authenticated: {detail}")]
    NotAuthenticated { tool: &'static str, detail: String },
    #[error("{what} failed: {detail}")]
    CommandFailed { what: String, detail: String },
    #[error("{what} timed out after {}s", COMMAND_TIMEOUT.as_secs())]
    Timeout { what: String },
    #[error("the command reported success but printed no pull request URL")]
    NoUrl,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Everything a proposal needs, already earned. Note there is no free-text
/// "description" field: the body is *derived* from the evidence, so no caller
/// can attach a narrative the claims don't support.
#[derive(Debug, Clone)]
pub struct ProposalRequest {
    pub failure: Failure,
    pub attribution: Option<Attribution>,
    pub reproduction: Option<Reproduction>,
    pub repair: Repair,
    /// The branch to merge into. `main` unless the caller knows better.
    pub base: String,
    /// The exact command a human would run to undo this if it is merged and
    /// goes wrong. Rendered verbatim; never executed here.
    pub revert_hint: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Proposal {
    pub url: String,
    pub branch: String,
    /// Exactly one claim: that this proposal exists, at this URL.
    pub claim: Claim,
}

#[async_trait]
pub trait ChangeProposal: Send + Sync {
    /// Stable name for the record and narration (`github`, ...).
    fn name(&self) -> &'static str;

    /// Publish `req.repair.branch` and open a proposal against `req.base`.
    ///
    /// Implementations MUST be idempotent enough to survive a retry: opening
    /// a proposal for a branch that already has one should return the
    /// existing one rather than erroring, since the alternative is a failure
    /// mode where a transient network error costs a repair its only surface.
    async fn propose(
        &self,
        repo: &Path,
        req: &ProposalRequest,
    ) -> Result<Proposal, ProposalError>;
}

// -- body rendering ----------------------------------------------------------


/// Clean up text an AGENT wrote before it goes into a title or body.
///
/// Found by reading the first real pull request this code opened. Codex's
/// summary was:
///
/// ```text
/// Fixed [server.js](/private/var/folders/.../drums-repro-01KYS.../server.js)
/// ```
///
/// which produced a title that leaked an absolute path from a temporary
/// worktree on the machine Drums happened to be running on, and then got
/// truncated mid-path. Two separate problems: the path is meaningless to every
/// reader and is an information leak about the host, and a markdown link in a
/// PR *title* renders as raw syntax.
///
/// So: collapse `[text](url)` to `text`, collapse whitespace, and leave the
/// rest alone. This deliberately does not try to rewrite prose — the agent's
/// wording is the agent's, and editing it would make the summary something
/// Drums authored.
fn scrub_agent_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '[' {
            // Collect up to the matching ']', then drop a following (...).
            let mut label = String::new();
            let mut closed = false;
            for c2 in chars.by_ref() {
                if c2 == ']' {
                    closed = true;
                    break;
                }
                label.push(c2);
            }
            if closed && chars.peek() == Some(&'(') {
                chars.next(); // consume '('
                let mut depth = 1;
                for c3 in chars.by_ref() {
                    if c3 == '(' {
                        depth += 1;
                    } else if c3 == ')' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                }
                out.push_str(&label);
            } else {
                out.push('[');
                out.push_str(&label);
                if closed {
                    out.push(']');
                }
            }
        } else {
            out.push(c);
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Longest title we will send. GitHub accepts more, but a title that wraps in
/// every list view is worse than one that ends in a word.
const MAX_TITLE_LEN: usize = 72;

fn truncate_on_word(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    match cut.rfind(' ') {
        Some(i) if i > max / 2 => format!("{}…", &cut[..i]),
        _ => format!("{cut}…"),
    }
}

fn chip_line(c: &Claim) -> String {
    format!("- `{}` — {}", c.provenance.chip(), c.text)
}

/// Every claim the pipeline earned for this failure, in the order it earned
/// them. This is the single place claim ordering is decided, so the PR body
/// and any future surface agree.
fn all_claims(req: &ProposalRequest) -> Vec<&Claim> {
    let mut out = vec![&req.failure.claim];
    if let Some(a) = &req.attribution {
        out.push(&a.claim);
    }
    if let Some(r) = &req.reproduction {
        out.extend(r.claims.iter());
    }
    out.extend(req.repair.claims.iter());
    out
}

/// The title. Deliberately boring and greppable.
pub fn render_title(req: &ProposalRequest) -> String {
    let summary = scrub_agent_text(&first_line(&req.repair.summary));
    let prefix = format!("fix({}): ", req.failure.service);
    let room = MAX_TITLE_LEN.saturating_sub(prefix.chars().count());
    format!("{prefix}{}", truncate_on_word(&summary, room))
}

/// The PR body: the evidence, in the order a reviewer needs it.
///
/// Ordering is the argument. A reviewer's first question is "what broke",
/// their second is "how do you know this fixes it", and their third is "what
/// don't you know". Unresolved claims therefore appear BEFORE the diff
/// summary, not in a footnote after it — a reviewer who stops reading early
/// must not stop having read only good news.
pub fn render_body(req: &ProposalRequest) -> String {
    let mut s = String::new();
    let claims = all_claims(req);
    let unresolved: Vec<&&Claim> = claims
        .iter()
        .filter(|c| c.provenance == Provenance::Unresolved)
        .collect();

    s.push_str("## What broke\n\n");
    s.push_str(&format!(
        "`{}` in `{}`{}\n\n",
        req.failure.signature.error_name,
        req.failure.signature.top_frame_file,
        req.failure
            .signature
            .top_frame_function
            .as_ref()
            .map(|f| format!(" · `{f}`"))
            .unwrap_or_default(),
    ));
    s.push_str(&format!(
        "Seen {} time{} in `{}`. Intake: **{}**.\n\n",
        req.failure.event_count,
        if req.failure.event_count == 1 { "" } else { "s" },
        req.failure.service,
        req.failure.intake.label(),
    ));

    if let Some(a) = &req.attribution {
        s.push_str("## Attributed to\n\n");
        s.push_str(&format!(
            "`{}` — {} (by {}), {} minute{} before the first error.\n\n",
            short_sha(&a.deploy.sha),
            a.deploy.description,
            a.deploy.author,
            a.minutes_after_deploy,
            if a.minutes_after_deploy == 1 { "" } else { "s" },
        ));
        if !a.overlap_files.is_empty() {
            s.push_str("Files that deploy touched, which the stack also names:\n\n");
            for f in &a.overlap_files {
                s.push_str(&format!("- `{f}`\n"));
            }
            s.push('\n');
        }
    }

    if let Some(r) = &req.reproduction {
        s.push_str("## Reproduction\n\n");
        s.push_str(&format!(
            "{} at `{}`{}\n\n",
            if r.reproduced {
                "Reproduced"
            } else {
                "NOT reproduced"
            },
            short_sha(&r.sha),
            match r.parent_clean {
                Some(true) =>
                    " — and the parent commit serves the same request cleanly, so this is the change that broke it.",
                Some(false) =>
                    " — but the parent commit fails the same way, so the attributed deploy is NOT the cause.",
                // `None` covers BOTH "not attempted" and "attempted and
                // failed" — the reason lands in an unresolved claim, not
                // here. "was not checked" would imply a choice not to look,
                // which is wrong in the failure case, so this says only what
                // is true of both: it is not established.
                None => " — whether the parent commit is clean was NOT established, so the attribution above is still only inferred.",
            },
        ));
    }

    // Before the diff, on purpose.
    if !unresolved.is_empty() {
        s.push_str("## What is NOT verified\n\n");
        s.push_str(
            "These are the parts Drums could not check by running something. A human has to.\n\n",
        );
        for c in &unresolved {
            s.push_str(&chip_line(c));
            s.push('\n');
        }
        s.push('\n');
    }

    s.push_str("## The repair\n\n");
    s.push_str(&format!("{}\n\n", scrub_agent_text(req.repair.summary.trim())));
    s.push_str(&format!(
        "Written by `{}` on `{}`.\n\n",
        req.repair.agent, req.repair.branch,
    ));
    // An empty fenced block renders as a grey void that looks like a rendering
    // bug. Some paths genuinely have no diff stat to show (a reported-issue
    // repair does not collect one), so omit the block rather than print an
    // empty one.
    let diff_stat = req.repair.diff_stat.trim();
    if !diff_stat.is_empty() {
        s.push_str(&format!("```\n{diff_stat}\n```\n\n"));
    }

    s.push_str("## Evidence\n\n");
    s.push_str("Every claim below, with how it is known:\n\n");
    for c in &claims {
        s.push_str(&chip_line(c));
        s.push('\n');
    }
    s.push('\n');
    s.push_str(
        "`verified` means Drums ran a check and observed the result. \
         `observed` means it saw it happen but did not re-run it. \
         `inferred` means it reasoned from timing and overlap. \
         `unresolved` means it could not tell — never that it is fine.\n\n",
    );

    if let Some(hint) = &req.revert_hint {
        s.push_str("## If this goes wrong\n\n");
        s.push_str(&format!("```\n{}\n```\n\n", hint.trim()));
    }

    s.push_str(&format!(
        "---\nOpened by Drums for failure `{}`. Nothing here was merged automatically.\n",
        req.failure.id,
    ));
    s
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("repair").trim().to_string()
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

// -- the GitHub implementation -----------------------------------------------

/// Opens pull requests by driving the team's own authenticated `gh` CLI.
#[derive(Debug, Default, Clone)]
pub struct GitHubProposal;

impl GitHubProposal {
    pub fn new() -> Self {
        Self
    }

    /// True when `gh` exists AND reports an authenticated account. Both
    /// matter: an installed-but-logged-out `gh` fails at the worst possible
    /// moment, after a repair has already been produced.
    pub async fn available() -> Result<(), ProposalError> {
        let out = run("gh", &["auth", "status"], None).await?;
        if out.status_ok {
            Ok(())
        } else {
            Err(ProposalError::NotAuthenticated {
                tool: "gh",
                detail: first_line(&out.merged()),
            })
        }
    }
}

#[async_trait]
impl ChangeProposal for GitHubProposal {
    fn name(&self) -> &'static str {
        "github"
    }

    async fn propose(
        &self,
        repo: &Path,
        req: &ProposalRequest,
    ) -> Result<Proposal, ProposalError> {
        let branch = req.repair.branch.clone();

        // Push the repair branch. `--set-upstream` so a human's later `git
        // push` from a checkout of it just works.
        let push = run(
            "git",
            &["push", "--set-upstream", "origin", &branch],
            Some(repo),
        )
        .await?;
        if !push.status_ok {
            let detail = push.merged();
            // Already-pushed is not a failure: a retry after a network error
            // must not cost the repair its proposal.
            if !detail.contains("Everything up-to-date") {
                return Err(ProposalError::CommandFailed {
                    what: format!("git push origin {branch}"),
                    detail: first_line(&detail),
                });
            }
        }

        // If a PR already exists for this branch, adopt it rather than
        // failing — same retry-safety argument.
        if let Some(existing) = existing_pr_url(repo, &branch).await? {
            return Ok(Proposal {
                claim: Claim {
                    text: format!("pull request already open at {existing}"),
                    provenance: Provenance::Observed,
                },
                url: existing,
                branch,
            });
        }

        let title = render_title(req);
        let body = render_body(req);
        let create = run(
            "gh",
            &[
                "pr",
                "create",
                "--base",
                &req.base,
                "--head",
                &branch,
                "--title",
                &title,
                "--body",
                &body,
            ],
            Some(repo),
        )
        .await?;

        if !create.status_ok {
            let detail = create.merged();
            if detail.contains("not logged into") || detail.contains("gh auth login") {
                return Err(ProposalError::NotAuthenticated {
                    tool: "gh",
                    detail: first_line(&detail),
                });
            }
            return Err(ProposalError::CommandFailed {
                what: "gh pr create".to_string(),
                detail: first_line(&detail),
            });
        }

        // Read the URL back from the command's own output rather than
        // constructing one — a URL we assembled ourselves would be a claim
        // about a page we never saw.
        let url = extract_pr_url(&create.merged()).ok_or(ProposalError::NoUrl)?;

        Ok(Proposal {
            claim: Claim {
                text: format!("pull request opened at {url}"),
                provenance: Provenance::Observed,
            },
            url,
            branch,
        })
    }
}

async fn existing_pr_url(repo: &Path, branch: &str) -> Result<Option<String>, ProposalError> {
    let out = run(
        "gh",
        &["pr", "list", "--head", branch, "--state", "open", "--json", "url"],
        Some(repo),
    )
    .await?;
    if !out.status_ok {
        // Not being able to LIST is not the same as there being none; treat
        // it as unknown and let the create attempt speak.
        return Ok(None);
    }
    Ok(extract_pr_url(&out.stdout))
}

/// Pull the first GitHub PR URL out of arbitrary command output.
///
/// Scans for the `https://` marker rather than splitting on whitespace: `gh
/// --json` prints `[{"url":"https://.../pull/7"}]` as a single token, so a
/// token-based reader finds nothing. A URL is read as running until the first
/// character that cannot appear in one.
pub fn extract_pr_url(text: &str) -> Option<String> {
    let mut rest = text;
    while let Some(at) = rest.find("https://") {
        let candidate: String = rest[at..]
            .chars()
            .take_while(|c| !c.is_whitespace() && !matches!(c, '"' | '\'' | '<' | '>' | '\\' | '}' | ']' | ','))
            .collect();
        if candidate.contains("/pull/") {
            return Some(candidate);
        }
        rest = &rest[at + "https://".len()..];
    }
    None
}

// -- process plumbing --------------------------------------------------------

struct Output {
    status_ok: bool,
    stdout: String,
    stderr: String,
}

impl Output {
    fn merged(&self) -> String {
        format!("{}\n{}", self.stdout.trim(), self.stderr.trim())
            .trim()
            .to_string()
    }
}

/// Run a command by argv. Never a shell — every argument here derives from
/// production data (a branch name built from a failure id, an agent-written
/// summary, an error message) and a shell would make each of them an
/// injection point.
async fn run(program: &str, args: &[&str], cwd: Option<&Path>) -> Result<Output, ProposalError> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProposalError::ToolMissing {
                tool: if program == "gh" { "gh" } else { "git" },
            })
        }
        Err(e) => return Err(ProposalError::Io(e)),
    };

    let what = format!("{program} {}", args.first().copied().unwrap_or(""));
    let out = match tokio::time::timeout(COMMAND_TIMEOUT, child.wait_with_output()).await {
        Ok(r) => r?,
        Err(_) => return Err(ProposalError::Timeout { what }),
    };

    Ok(Output {
        status_ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    })
}
