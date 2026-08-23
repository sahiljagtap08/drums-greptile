//! The hosted seam: handing a locally-reproduced failure to the control plane.
//!
//! ## What this is, and what it deliberately is not
//!
//! `drums watch` detects, attributes, reproduces and repairs on the machine it
//! runs on. That is the whole loop, and it stops when the laptop closes.
//! `docs/RUNNER.md` §2 splits it: the control plane decides *what* should
//! happen, the customer's CI does the work that needs their code and their
//! credentials. This module is the wire between the two halves — the one thing
//! that was missing, because `POST /api/repairs` has always worked and nothing
//! ever called it.
//!
//! What moves is the REPAIR, and only the repair. Detection, attribution and —
//! critically — reproduction all still happen here, on this machine, before
//! anything is sent. See [`RemoteRepairs::dispatch`] for why that ordering is
//! not a nicety.
//!
//! ## Why the body is shaped the way it is
//!
//! The console stores `instruction` verbatim and hands it back, byte for byte,
//! to the runner over `POST /api/repairs/instruction`. The runner writes it to
//! a file and runs `drums repair --input <file>`, which deserialises it as
//! [`crate::repair_cmd::RepairInput`]. So `instruction` is not "some JSON the
//! console understands" — it is exactly that struct, and the two must never
//! drift. [`Instruction`] exists to make that an explicit, tested claim rather
//! than an accident of two files agreeing today.
//!
//! ## The credential
//!
//! The bearer token comes from `drums login` and is read from
//! `~/.drums/credentials.toml`. It is never printed, never logged, never put in
//! an error message, and [`RemoteRepairs`] has no `Debug` derive that could
//! leak it into a `tracing` field or a panic message by accident.

use std::path::{Path, PathBuf};
use std::time::Duration;

use engine_core::authority::{ship_decision, Rung, ShipDecision};
use engine_core::{Attribution, Failure, Reproduction};
use serde::Serialize;

/// How long to wait on the console before giving up and carrying on watching.
///
/// Short on purpose. This call is on the path between "a failure reproduced"
/// and "the operator is told what happened to it", and a control plane that has
/// gone away must not hold that open — the local loop keeps running either way.
const DISPATCH_TIMEOUT: Duration = Duration::from_secs(20);

/// The console truncates `acceptance` to 20 strings before it stores anything.
/// Capping here as well is not belt-and-braces: the criteria travel TWICE (once
/// at the top level, which becomes a workflow input, and once inside
/// `instruction`, which is what the agent actually reads), and letting the
/// console truncate one copy and not the other would show a human a shorter
/// list than the agent was given.
const ACCEPTANCE_MAX: usize = 20;

/// What the ladder is being asked about. The console records this next to the
/// approval so a person answering it knows what they are consenting to.
///
/// `repair.dispatch`, not `repair` or `ship`: consenting to a repair being
/// STARTED on somebody else's runners, with their agent credential and their
/// CI minutes, is a different question from consenting to one being deployed.
pub const ACTION_CLASS: &str = "repair.dispatch";

/// Refusal text for the one rule this module exists to hold. Public so the test
/// that proves the rule asserts on the same string the operator sees.
pub const NOT_REPRODUCED: &str =
    "refusing to dispatch: this failure did not reproduce at the attributed revision, and Drums \
     only repairs failures it made happen";

/// Refusal text for an intake that can never have been reproduced.
pub const NOT_REPLAYABLE: &str =
    "refusing to dispatch: this failure carries no replayable request, so no reproduction could \
     ever have run for it";

/// A human has to answer before this repair starts.
///
/// Serialises straight into the console's `requires_approval`, which is what
/// makes it create an approval, notify whoever is on the account, hold the job
/// at `queued`, and return an `approval_url` instead of dispatching anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApprovalAsk {
    pub reason: String,
    pub action_class: String,
}

/// EXACTLY [`crate::repair_cmd::RepairInput`], on the wire.
///
/// Borrowed rather than owned because the caller already has both halves and a
/// `Failure` carries a whole error event; there is no reason to clone it to
/// serialise it.
///
/// `job_id` is `None` and cannot be anything else: the console mints the job id
/// AFTER it has stored this object, and stores it verbatim. See the note on
/// `job_id` in `docs`-facing terms in the module docs of `repair_cmd`; the
/// outcome artifact is bound to its job by the workflow run id GitHub reports,
/// not by this field, so a null here costs nothing.
#[derive(Debug, Clone, Serialize)]
struct Instruction<'a> {
    failure: &'a Failure,
    attribution: &'a Attribution,
    job_id: Option<String>,
    acceptance: &'a [String],
    /// The boot recipe reproduction ran with HERE, in the repro contract
    /// (whitespace argv, `{port}` token). Sent because the runner otherwise
    /// guesses `node <entry>`, and the first real approved repair — a FastAPI
    /// service — died on that guess. Omitted when this watch used the guess
    /// itself, which is exactly when the runner's guess is also right.
    #[serde(skip_serializing_if = "Option::is_none")]
    boot_cmd: Option<&'a str>,
    /// What the record remembers, computed at dispatch time — the runner's
    /// own record is a throwaway with no history, so memory must travel.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    remembered: Vec<String>,
}

/// The body `POST /api/repairs` reads.
///
/// Field for field against `console/app/api/repairs/route.ts`. `repository`,
/// `failure_id` and `rev` are the three it refuses without.
#[derive(Debug, Clone, Serialize)]
struct DispatchBody<'a> {
    repository: &'a str,
    failure_id: &'a str,
    rev: &'a str,
    acceptance: &'a [String],
    instruction: Instruction<'a>,
    /// The branch `workflow_dispatch` resolves against — NOT the failing sha.
    /// GitHub looks the workflow file up on a ref, and the failing sha is what
    /// the workflow then checks out, passed as an input.
    default_branch: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    requires_approval: Option<ApprovalAsk>,
}

/// What the console said when it accepted the job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    pub job_id: String,
    /// `Some` when the ladder said a person has to answer first. The job exists
    /// and is `queued`; nothing has run in anybody's CI.
    pub approval_url: Option<String>,
    pub expires_at: Option<String>,
}

impl Accepted {
    pub fn is_awaiting_approval(&self) -> bool {
        self.approval_url.is_some()
    }
}

/// Everything a dispatch needs, resolved once at startup.
///
/// Deliberately NO `#[derive(Debug)]`. This struct holds a bearer token, and a
/// derived `Debug` is how a credential ends up in a `tracing` field, a panic
/// message, or an `{:?}` somebody added while debugging something else.
pub struct RemoteRepairs {
    console_url: String,
    /// Never printed, never logged, never included in an error.
    token: String,
    /// `owner/name`, as the console spells it in `repositories.slug`.
    repository: String,
    default_branch: String,
    /// Where the authority ladder is folded from. The rung is REBUILT from the
    /// append-only record on every dispatch rather than cached at startup, so a
    /// demotion recorded ten minutes into a watch takes effect on the next
    /// failure and not on the next restart.
    record_path: PathBuf,
    /// The highest rung the operator consented to (`--repair propose` /
    /// `--repair auto`). Never the rung itself — see [`Self::approval_ask`].
    ceiling: Rung,
    /// The watch's own boot recipe, forwarded in every instruction so the
    /// runner reproduces the way this machine did. See `Instruction::boot_cmd`.
    boot_cmd: Option<String>,
    client: reqwest::Client,
}

impl RemoteRepairs {
    /// Build from the credentials `drums login` stored.
    ///
    /// Every failure here is a startup failure, reported by name, because the
    /// alternative is discovering it after a failure has already been detected
    /// and reproduced — at which point the work is done and there is nowhere to
    /// send it.
    pub fn from_login(repo: &Path, record_path: PathBuf, ceiling: Rung) -> Result<Self, String> {
        let creds = crate::login::load()?.ok_or_else(|| {
            "not signed in — run `drums login` before watching with --dispatch-repairs".to_string()
        })?;
        let console_url = crate::login::console_url();
        // A token minted against a staging console is not a token for
        // production. `drums login` records which one it came from precisely so
        // this can be checked rather than discovered as a confusing 401.
        if !creds.console_url.is_empty() && creds.console_url != console_url {
            return Err(format!(
                "the stored credential is for {} but this run would talk to {console_url} — run `drums login` again",
                creds.console_url
            ));
        }
        let repository = repo_slug(repo).ok_or_else(|| {
            format!(
                "could not work out which GitHub repository {} is — `git remote get-url origin` \
                 gave nothing usable, and the control plane needs `owner/name` to find your \
                 installation",
                repo.display()
            )
        })?;
        let client = reqwest::Client::builder()
            .timeout(DISPATCH_TIMEOUT)
            .build()
            .map_err(|e| format!("could not build an HTTP client: {e}"))?;
        Ok(Self {
            console_url,
            token: creds.token,
            repository,
            default_branch: default_branch(repo),
            record_path,
            ceiling,
            boot_cmd: None,
            client,
        })
    }

    /// For tests and for `--console-url`-style overrides: everything explicit,
    /// nothing read from disk.
    pub fn new(
        console_url: impl Into<String>,
        token: impl Into<String>,
        repository: impl Into<String>,
        default_branch: impl Into<String>,
        record_path: PathBuf,
        ceiling: Rung,
    ) -> Result<Self, String> {
        Ok(Self {
            console_url: console_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            repository: repository.into(),
            default_branch: default_branch.into(),
            record_path,
            ceiling,
            boot_cmd: None,
            client: reqwest::Client::builder()
                .timeout(DISPATCH_TIMEOUT)
                .build()
                .map_err(|e| format!("could not build an HTTP client: {e}"))?,
        })
    }

    /// Attach the watch's boot recipe, forwarded in every instruction.
    /// A builder rather than a constructor argument so the two constructors
    /// stay honest about what they read (disk, or nothing).
    pub fn with_boot(mut self, boot_cmd: Option<String>) -> Self {
        self.boot_cmd = boot_cmd;
        self
    }

    pub fn console_url(&self) -> &str {
        &self.console_url
    }

    pub fn repository(&self) -> &str {
        &self.repository
    }

    /// What the autonomy ladder says about STARTING this repair remotely.
    ///
    /// `None` means go; `Some` means send `requires_approval` and let the
    /// console put the question in front of a person.
    ///
    /// # Why the bar here is act-alone
    ///
    /// Locally, `Rung::Propose` already permits a repair to be produced: an
    /// agent edits a throwaway worktree while the operator watches it happen in
    /// their own terminal, and the ladder only gates the SHIP. Consent is
    /// continuous there because a person is sitting in front of it.
    ///
    /// A dispatched repair is not that. It runs on the customer's runners, on
    /// their bill, with their agent credential, and it can end as a pull
    /// request that nobody at this terminal ever sees. Nothing about it is
    /// attended. So the rung that governs it is the rung that governs
    /// unattended action — act-alone — and everything below it asks.
    ///
    /// Note what is NOT re-derived here: the intake gate. It is evaluated by
    /// [`ship_decision`], the same single function the local pipeline calls, so
    /// there is exactly one place in this codebase where authority is decided.
    pub fn approval_ask(
        &self,
        failure: &Failure,
        reproduction: &Reproduction,
    ) -> Option<ApprovalAsk> {
        let class =
            engine_authority::FailureClass::new(&failure.service, &failure.signature.error_name);
        let earned = match engine_authority::Ladder::load(&self.record_path) {
            Ok(l) => l.rung(&class),
            // A record we cannot read is not permission to act. Fall back to the
            // safe rung rather than to the operator's ceiling — the same
            // decision `run_repair_pipeline` makes for the same reason.
            Err(_) => Rung::Propose,
        };
        let effective = crate::engine::min_rung(self.ceiling, earned);

        // The evidence is the LOCAL reproduction's claims — firsthand, observed
        // by this process, which is the strongest thing anything in this
        // codebase can present. It is also the only evidence that exists at
        // dispatch time: the repair has not been written yet.
        let evidence = crate::engine::LocalEvidence {
            claims: &reproduction.claims,
        };
        match ship_decision(effective, &failure.intake, &evidence) {
            ShipDecision::MayShip => None,
            ShipDecision::Propose(reason) => Some(ApprovalAsk {
                reason: format!(
                    "Drums reproduced {} in {} and wants to repair it in your CI. {}",
                    failure.signature.error_name,
                    failure.service,
                    reason.withheld_text()
                ),
                action_class: ACTION_CLASS.to_string(),
            }),
        }
    }

    /// Hand one reproduced failure to the control plane.
    ///
    /// # The precondition, and why it is checked HERE
    ///
    /// A repair is only ever attempted against a failure Drums made happen
    /// again. That is the product, not a quality bar: without it, a "repair" is
    /// a patch for a program nobody watched break, and every claim downstream
    /// of it — including the `verified` the ship gate reads — is describing
    /// something that was never observed.
    ///
    /// Reproduction is the one stage that CANNOT move to the control plane at
    /// this seam, because the whole point of dispatching is that the runner
    /// rebuilds and repairs; if Drums also trusted the runner to decide whether
    /// the failure was real, nothing local would be left to disbelieve it. So
    /// reproduction stays on this machine, it runs FIRST, and its result is a
    /// required argument here.
    ///
    /// The check is inside this function rather than at the call site
    /// deliberately. A guard beside the path is not enforcement — it survives
    /// exactly as long as nobody adds a second caller. Put here, the HTTP
    /// request is physically behind it, and the test that removes the guard
    /// fails by reaching the network instead of returning this message.
    pub async fn dispatch(
        &self,
        failure: &Failure,
        attribution: &Attribution,
        reproduction: &Reproduction,
        acceptance: &[String],
    ) -> Result<Accepted, String> {
        if !failure.intake.is_replayable() {
            return Err(NOT_REPLAYABLE.to_string());
        }
        if !reproduction.reproduced {
            return Err(NOT_REPRODUCED.to_string());
        }

        // The runner checks out `rev` and `drums repair` builds its worktree at
        // `attribution.deploy.sha`. If those two ever disagreed, the repair
        // would be produced against different code from the one that was
        // checked out — so there is one source for both, and it is the
        // attribution.
        let rev = attribution.deploy.sha.trim();
        if !is_hex_sha(rev) {
            return Err(format!(
                "refusing to dispatch: `{rev}` is not a commit sha, and the control plane keys \
                 the whole job on it"
            ));
        }

        let acceptance = &acceptance[..acceptance.len().min(ACCEPTANCE_MAX)];
        let ask = self.approval_ask(failure, reproduction);
        // Memory for a runner that has none. Unreadable record = honest
        // emptiness; a dispatch must not die for want of advice.
        let remembered = engine_record::read_all(&self.record_path)
            .map(|read| {
                engine_recall::for_failure(
                    &read.lines,
                    &failure.signature.error_name,
                    &failure.signature.top_frame_file,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0),
                )
            })
            .unwrap_or_default();
        let body = DispatchBody {
            repository: &self.repository,
            failure_id: &failure.id,
            rev,
            acceptance,
            // The captured request travels RAW and unredacted, which is the one
            // exception to "Drums moves the record, not the material" —
            // `supabase/migrations/0015_repair_instruction.sql` says so in as
            // many words, and clears it the moment the job is terminal. A
            // runner cannot replay a request it was never given, and a redacted
            // body would reproduce a different request from the one that failed.
            instruction: Instruction {
                failure,
                attribution,
                job_id: None,
                acceptance,
                boot_cmd: self.boot_cmd.as_deref(),
                remembered,
            },
            default_branch: &self.default_branch,
            requires_approval: ask,
        };

        let url = format!("{}/api/repairs", self.console_url);
        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            // `e` is a reqwest error over a URL we built; it cannot contain the
            // token, which travels in a header reqwest does not format here.
            .map_err(|e| format!("could not reach {}: {e}", self.console_url))?;

        let status = response.status().as_u16();
        let parsed: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
        interpret(status, &parsed)
    }
}

/// Decide what a `POST /api/repairs` response means.
///
/// Split out and pure, exactly like `login::interpret_poll`, because these are
/// the branches that decide whether an operator is told "it is running", "go
/// and approve it", or "sign in again" — and every one of them has to be
/// exercisable without a server.
pub fn interpret(status: u16, body: &serde_json::Value) -> Result<Accepted, String> {
    let text = |key: &str| body.get(key).and_then(|v| v.as_str()).map(str::to_string);

    if (200..300).contains(&status) {
        let Some(job_id) = text("job_id").filter(|s| !s.is_empty()) else {
            // A 2xx with no job id is a server bug. Saying so beats reporting a
            // dispatch that may or may not have happened.
            return Err(
                "the console accepted the repair but sent no job id — nothing can be followed up"
                    .to_string(),
            );
        };
        return Ok(Accepted {
            approval_url: text("approval_url").filter(|s| !s.is_empty()),
            expires_at: text("expires_at"),
            job_id,
        });
    }

    let error = text("error").unwrap_or_default();
    let message = text("message").or_else(|| text("error_description"));

    Err(match (status, error.as_str()) {
        (401, _) => {
            "the console did not accept this machine's credential — run `drums login`".to_string()
        }
        (_, "repository_not_connected") => message.unwrap_or_else(|| {
            "that repository is not connected to this account — connect it at app.drums.sh/settings"
                .to_string()
        }),
        (_, "no_installation") => message.unwrap_or_else(|| {
            "that repository has no GitHub installation — reconnect it in settings".to_string()
        }),
        (_, "dispatch_failed") => {
            let detail = message.unwrap_or_else(|| "GitHub refused the dispatch".to_string());
            format!(
                "the control plane could not start the workflow: {detail} — is \
                 .github/workflows/drums-repair.yml committed on the default branch?"
            )
        }
        (_, "invalid_rev") => {
            message.unwrap_or_else(|| "the console refused the revision".to_string())
        }
        (_, "invalid_request") => {
            message.unwrap_or_else(|| "the console refused the request".to_string())
        }
        // An unrecognised refusal is still a refusal. Repeat what the server
        // said rather than inventing a reason for it.
        (_, other) => match (message, other.is_empty()) {
            (Some(m), _) => format!("the console refused the repair: {m}"),
            (None, false) => format!("the console refused the repair: {other}"),
            (None, true) => format!("the console refused the repair (HTTP {status})"),
        },
    })
}

/// A commit sha as the console's own check spells it: `^[0-9a-f]{7,40}$`.
fn is_hex_sha(s: &str) -> bool {
    (7..=40).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `owner/name` from this repo's `origin` remote.
pub fn repo_slug(repo: &Path) -> Option<String> {
    parse_slug(&origin_url(repo)?)
}

/// What `git remote get-url origin` prints, or `None` when there is no
/// remote (or no git at all). Split out so record sync (`crate::sync`) reads
/// the remote through the same call rather than a second spelling of it.
pub(crate) fn origin_url(repo: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The pure half of [`repo_slug`]. Handles the three spellings git hands back:
/// `git@host:owner/name.git`, `https://host/owner/name.git`, and
/// `ssh://git@host/owner/name`.
///
/// Returns `None` rather than a guess for anything else. A wrong slug is not a
/// harmless typo — the console looks the repository up scoped to the account,
/// so it becomes "that repository is not connected", which reads as a
/// configuration problem the operator does not have.
pub fn parse_slug(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    let rest = match url.split_once("://") {
        // scheme://[user@]host/owner/name
        Some((_, after)) => after.split_once('/').map(|(_, p)| p)?,
        // scp-like: [user@]host:owner/name
        None => url.split_once(':').map(|(_, p)| p)?,
    };
    let rest = rest.trim_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/').filter(|s| !s.is_empty());
    let owner = parts.next()?;
    let name = parts.next()?;
    if parts.next().is_some() || owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

/// The branch `workflow_dispatch` resolves the workflow file against.
///
/// Falls back to `main` — the same default the console applies — rather than
/// failing, because a repository whose `origin/HEAD` is not set locally is
/// common and is not a reason to refuse to dispatch anything.
pub fn default_branch(repo: &Path) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .output();
    if let Ok(out) = out {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some(branch) = text.trim().strip_prefix("origin/") {
                if !branch.is_empty() {
                    return branch.to_string();
                }
            }
        }
    }
    "main".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::{Claim, DeployRecord, ErrorEvent, ErrorSignature, Intake, Provenance};
    use serde_json::json;

    fn failure(intake: Intake) -> Failure {
        Failure {
            id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            service: "shop".into(),
            signature: ErrorSignature {
                error_name: "TypeError".into(),
                top_frame_file: "server.js".into(),
                top_frame_function: None,
            },
            first_seen_ms: 1,
            event_count: 3,
            sample: ErrorEvent {
                service: "shop".into(),
                occurred_at_ms: 1,
                error_name: "TypeError".into(),
                error_message: "boom".into(),
                stack: "TypeError: boom\n    at computeTotal (server.js:4:2)".into(),
                request: Some(engine_core::CapturedRequest {
                    method: "POST".into(),
                    path: "/api/checkout".into(),
                    content_type: Some("application/json".into()),
                    body: Some(r#"{"sku":"a"}"#.into()),
                }),
                intake: intake.clone(),
            },
            intake,
            claim: Claim {
                text: "3 errors".into(),
                provenance: Provenance::Observed,
            },
        }
    }

    fn attribution() -> Attribution {
        Attribution {
            deploy: DeployRecord {
                sha: "deadbeef0123456".into(),
                description: "c1".into(),
                author: "t".into(),
                deployed_at_ms: 1,
            },
            overlap_files: vec!["server.js".into()],
            minutes_after_deploy: 2,
            claim: Claim {
                text: "1 file overlaps".into(),
                provenance: Provenance::Inferred,
            },
        }
    }

    fn reproduction(reproduced: bool) -> Reproduction {
        Reproduction {
            sha: "deadbeef0123456".into(),
            reproduced,
            parent_clean: Some(true),
            detail: "replayed".into(),
            claims: vec![Claim {
                text: "replayed the captured request and it failed".into(),
                provenance: Provenance::Verified,
            }],
        }
    }

    /// A console that cannot exist. Any request actually leaving this process
    /// fails against it, which is what makes the refusal tests below meaningful:
    /// they assert the REFUSAL text, so a removed guard shows up as a connection
    /// error rather than a silently passing test.
    fn unreachable_console(ceiling: Rung) -> RemoteRepairs {
        RemoteRepairs::new(
            // Reserved for documentation (RFC 6761), port 1: never routable.
            "http://192.0.2.0:1",
            "drums_pat_this_must_never_be_sent",
            "acme/api",
            "main",
            PathBuf::from("/nonexistent/record.jsonl"),
            ceiling,
        )
        .expect("the client must build")
    }

    // -- the one rule ---------------------------------------------------------

    /// THE product property. A repair is only ever attempted against a failure
    /// Drums made happen again; a reproduction that came back `false` means the
    /// attribution is wrong, and repairing on it produces a change for the wrong
    /// reason.
    #[tokio::test]
    async fn a_failure_that_did_not_reproduce_is_never_dispatched() {
        let remote = unreachable_console(Rung::ActAlone);
        let err = remote
            .dispatch(
                &failure(Intake::Snippet),
                &attribution(),
                &reproduction(false),
                &["x".to_string()],
            )
            .await
            .expect_err("a failure that did not reproduce must not be dispatched");
        assert_eq!(
            err, NOT_REPRODUCED,
            "the refusal must come from the guard, not from the network — if this is a \
             connection error, the guard is gone and unreproduced failures are being sent"
        );
    }

    /// Defence in depth for the same rule from the other side: a trigger or
    /// reported intake never had a request to replay, so no reproduction could
    /// have run for it at all. The engine's intake fork already skips
    /// reproduction for these; this makes sending one impossible rather than
    /// merely unreachable.
    #[tokio::test]
    async fn a_failure_with_no_replayable_request_is_never_dispatched() {
        let remote = unreachable_console(Rung::ActAlone);
        for intake in [
            Intake::Trigger {
                source: "hyperdx".into(),
            },
            Intake::Reported {
                source: "linear".into(),
            },
        ] {
            let err = remote
                .dispatch(
                    &failure(intake.clone()),
                    &attribution(),
                    &reproduction(true),
                    &[],
                )
                .await
                .expect_err("a non-replayable intake must not be dispatched");
            assert_eq!(err, NOT_REPLAYABLE, "{intake:?}");
        }
    }

    /// The runner checks out the top-level `rev` and `drums repair` builds its
    /// worktree at `attribution.deploy.sha`. A non-sha would be refused by the
    /// console anyway; refusing it here means the operator is told which of
    /// their own values is wrong instead of reading a 400.
    #[tokio::test]
    async fn a_revision_that_is_not_a_commit_sha_is_refused_before_anything_is_sent() {
        let remote = unreachable_console(Rung::ActAlone);
        let mut attr = attribution();
        attr.deploy.sha = "HEAD".into();
        let err = remote
            .dispatch(&failure(Intake::Snippet), &attr, &reproduction(true), &[])
            .await
            .expect_err("a non-sha revision must be refused");
        assert!(err.contains("not a commit sha"), "{err}");
    }

    // -- the wire body --------------------------------------------------------

    /// The console stores `instruction` verbatim and hands it back to the
    /// runner, which deserialises it as `RepairInput`. If these two ever drift,
    /// every hosted repair fails at `drums repair --input` with a parse error
    /// nobody watching the console can diagnose.
    #[test]
    fn the_instruction_round_trips_into_a_repair_input() {
        let f = failure(Intake::Snippet);
        let a = attribution();
        let acceptance = vec!["POST /api/checkout returns 2xx".to_string()];
        let body = DispatchBody {
            repository: "acme/api",
            failure_id: &f.id,
            rev: &a.deploy.sha,
            acceptance: &acceptance,
            instruction: Instruction {
                failure: &f,
                attribution: &a,
                job_id: None,
                acceptance: &acceptance,
                boot_cmd: Some("python3 -m uvicorn app:app --port {port}"),
                remembered: vec![
                    "A past repair (names app.py) was shipped and later REVERTED 3 days ago."
                        .to_string(),
                ],
            },
            default_branch: "main",
            requires_approval: None,
        };

        let wire = serde_json::to_value(&body).expect("the body must serialise");

        // The console's three required fields, spelled its way.
        assert_eq!(wire["repository"], "acme/api");
        assert_eq!(wire["failure_id"], f.id);
        assert_eq!(wire["rev"], a.deploy.sha);
        assert_eq!(wire["default_branch"], "main");
        assert!(
            wire.get("requires_approval").is_none(),
            "an absent ask must be absent, not null — the console tests `body?.requires_approval`"
        );

        // And the instruction is a RepairInput, proven by parsing it as one.
        let input: crate::repair_cmd::RepairInput =
            serde_json::from_value(wire["instruction"].clone())
                .expect("the instruction the runner receives must deserialise as RepairInput");
        assert_eq!(
            input.remembered,
            vec![
                "A past repair (names app.py) was shipped and later REVERTED 3 days ago."
                    .to_string()
            ],
            "memory must survive the wire exactly — the runner has no record to rebuild it from"
        );
        assert_eq!(input.failure.id, f.id);
        assert_eq!(input.failure.signature.error_name, "TypeError");
        assert_eq!(input.attribution.deploy.sha, a.deploy.sha);
        assert_eq!(input.acceptance, acceptance);
        assert!(
            input.job_id.is_none(),
            "the console mints the job id after storing this, so it cannot be set here"
        );
        // The captured request has to survive: it is the only reason the
        // instruction exists at all.
        let req = input
            .failure
            .replayable_request()
            .expect("the failing request must reach the runner or it cannot reproduce");
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/api/checkout");
    }

    #[test]
    fn an_approval_ask_serialises_the_way_the_console_reads_it() {
        let ask = ApprovalAsk {
            reason: "because".into(),
            action_class: ACTION_CLASS.to_string(),
        };
        let v = serde_json::to_value(&ask).unwrap();
        assert_eq!(v["reason"], "because");
        assert_eq!(v["action_class"], "repair.dispatch");
    }

    // -- responses ------------------------------------------------------------

    #[test]
    fn a_dispatched_job_is_reported_with_its_id_and_no_approval() {
        let a = interpret(
            200,
            &json!({"ok": true, "job_id": "j-1", "status": "dispatched"}),
        )
        .expect("a 200 is an acceptance");
        assert_eq!(a.job_id, "j-1");
        assert!(!a.is_awaiting_approval());
    }

    #[test]
    fn an_approval_response_carries_the_url_the_operator_has_to_be_shown() {
        let a = interpret(
            200,
            &json!({
                "ok": true,
                "job_id": "j-2",
                "status": "awaiting_approval",
                "approval_url": "https://app.drums.sh/approvals/abc",
                "expires_at": "2026-08-03T12:00:00Z"
            }),
        )
        .expect("an approval is still an acceptance — the job exists");
        assert!(a.is_awaiting_approval());
        assert_eq!(
            a.approval_url.as_deref(),
            Some("https://app.drums.sh/approvals/abc")
        );
        assert_eq!(a.expires_at.as_deref(), Some("2026-08-03T12:00:00Z"));
    }

    /// A 401 is the single most likely thing an operator will hit — a token
    /// that expired, or was revoked from the console. It must say what to do,
    /// and it must not panic: `watch` keeps running through it.
    #[test]
    fn a_401_says_to_sign_in_again_and_does_not_panic() {
        let err = interpret(
            401,
            &json!({"error": "unauthorized", "error_description": "run `drums login`"}),
        )
        .expect_err("401 is a refusal");
        assert!(err.contains("drums login"), "{err}");

        // The same, with nothing usable in the body at all.
        let err = interpret(401, &serde_json::Value::Null).expect_err("401 is a refusal");
        assert!(err.contains("drums login"), "{err}");
    }

    #[test]
    fn every_refusal_the_console_can_send_produces_an_actionable_sentence() {
        let cases = [
            (
                404,
                json!({"error": "repository_not_connected", "message": "acme/api is not connected to this account."}),
            ),
            (
                409,
                json!({"error": "no_installation", "message": "That repository has no GitHub installation."}),
            ),
            (
                502,
                json!({"error": "dispatch_failed", "message": "workflow not found", "job_id": "j"}),
            ),
            (
                400,
                json!({"error": "invalid_rev", "message": "rev must be a commit sha"}),
            ),
            (
                400,
                json!({"error": "invalid_request", "message": "repository, failure_id and rev are required"}),
            ),
            (500, json!({"error": "server_error"})),
            (503, json!({})),
            (418, serde_json::Value::Null),
        ];
        for (status, body) in cases {
            let err = interpret(status, &body).expect_err("{status} is a refusal");
            assert!(!err.is_empty(), "HTTP {status} produced an empty reason");
            assert!(
                !err.contains("drums_pat_"),
                "no refusal may ever quote a credential: {err}"
            );
        }
        // The unrecognised ones still name something the operator can act on.
        assert!(interpret(503, &json!({})).unwrap_err().contains("503"));
        assert!(interpret(500, &json!({"error": "server_error"}))
            .unwrap_err()
            .contains("server_error"));
    }

    /// A 2xx with no job id is a server bug, and storing "it worked" for a job
    /// nobody can follow up is worse than saying so.
    #[test]
    fn a_success_without_a_job_id_is_refused() {
        let err = interpret(200, &json!({"ok": true})).expect_err("no job id is not a success");
        assert!(err.contains("no job id"), "{err}");
        assert!(interpret(201, &json!({"ok": true, "job_id": ""})).is_err());
    }

    // -- the ladder -----------------------------------------------------------

    /// The default ceiling is `propose`, so the default hosted behaviour is to
    /// ask. That is the point: a repair running on somebody else's runners with
    /// nobody watching this terminal is an unattended action, and unattended
    /// actions need act-alone.
    #[test]
    fn a_propose_ceiling_always_asks_a_human_first() {
        let remote = unreachable_console(Rung::Propose);
        let ask = remote
            .approval_ask(&failure(Intake::Snippet), &reproduction(true))
            .expect("propose must ask");
        assert_eq!(ask.action_class, ACTION_CLASS);
        assert!(ask.reason.contains("TypeError"), "{}", ask.reason);
        assert!(ask.reason.contains("shop"), "{}", ask.reason);
        assert!(ask.reason.contains("not act-alone"), "{}", ask.reason);
    }

    /// The operator consented to act-alone and the class (unknown to an
    /// unreadable record, so `Propose`)... still is not act-alone. The ceiling
    /// alone never grants authority — that is the whole of "autonomy is earned,
    /// not configured", and it holds in hosted mode too.
    #[test]
    fn an_auto_ceiling_alone_does_not_skip_the_human() {
        let remote = unreachable_console(Rung::ActAlone);
        assert!(
            remote
                .approval_ask(&failure(Intake::Snippet), &reproduction(true))
                .is_some(),
            "a class that has earned nothing must still ask, whatever the operator consented to"
        );
    }

    /// The class has earned act-alone in the record AND the operator consented.
    /// Only then does a dispatch go straight through.
    #[test]
    fn an_earned_act_alone_class_dispatches_without_asking() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("record.jsonl");
        engine_authority::promote(&record, "shop/TypeError", 1).expect("promote must write");

        let remote = RemoteRepairs::new(
            "http://192.0.2.0:1",
            "drums_pat_x",
            "acme/api",
            "main",
            record.clone(),
            Rung::ActAlone,
        )
        .unwrap();
        assert!(
            remote
                .approval_ask(&failure(Intake::Snippet), &reproduction(true))
                .is_none(),
            "an earned act-alone class with operator consent needs no approval"
        );

        // And the operator's ceiling still wins: `--repair propose` opts out
        // however much the class has earned.
        let capped = RemoteRepairs::new(
            "http://192.0.2.0:1",
            "drums_pat_x",
            "acme/api",
            "main",
            record,
            Rung::Propose,
        )
        .unwrap();
        assert!(
            capped
                .approval_ask(&failure(Intake::Snippet), &reproduction(true))
                .is_some(),
            "an operator can always opt out"
        );
    }

    /// A reproduction that established nothing cannot authorise an unattended
    /// dispatch even for an act-alone class — the same evidence check the local
    /// ship gate applies, reached through the same function.
    #[test]
    fn a_reproduction_with_no_verified_claim_still_asks() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("record.jsonl");
        engine_authority::promote(&record, "shop/TypeError", 1).unwrap();
        let remote = RemoteRepairs::new(
            "http://192.0.2.0:1",
            "drums_pat_x",
            "acme/api",
            "main",
            record,
            Rung::ActAlone,
        )
        .unwrap();

        let mut repro = reproduction(true);
        repro.claims = vec![Claim {
            text: "the replay could not be observed".into(),
            provenance: Provenance::Unresolved,
        }];
        let ask = remote
            .approval_ask(&failure(Intake::Snippet), &repro)
            .expect("nothing established must not act alone");
        assert!(ask.reason.contains("unresolved"), "{}", ask.reason);
    }

    // -- slug + branch --------------------------------------------------------

    #[test]
    fn every_spelling_git_hands_back_yields_the_same_slug() {
        for url in [
            "git@github.com:acme/api.git",
            "git@github.com:acme/api",
            "https://github.com/acme/api.git",
            "https://github.com/acme/api",
            "ssh://git@github.com/acme/api.git",
            "https://user:token@github.com/acme/api.git",
            "  https://github.com/acme/api/  ",
        ] {
            assert_eq!(parse_slug(url).as_deref(), Some("acme/api"), "{url}");
        }
    }

    /// A guessed slug becomes "that repository is not connected", which reads
    /// as a configuration problem the operator does not have. `None` makes it a
    /// named startup failure instead.
    #[test]
    fn anything_that_is_not_owner_slash_name_is_refused_rather_than_guessed() {
        for url in [
            "",
            "   ",
            "/srv/local/repo",
            "https://github.com/acme",
            "https://github.com/acme/api/extra",
            "git@github.com:",
        ] {
            assert_eq!(parse_slug(url), None, "{url:?} must not produce a slug");
        }
    }

    #[test]
    fn a_repository_with_no_origin_head_still_gets_a_branch() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(default_branch(dir.path()), "main");
    }

    #[test]
    fn the_sha_check_matches_the_consoles_own() {
        assert!(is_hex_sha("deadbee"));
        assert!(is_hex_sha(&"a".repeat(40)));
        assert!(!is_hex_sha("deadbe"), "six is below the console's minimum");
        assert!(!is_hex_sha(&"a".repeat(41)));
        assert!(
            !is_hex_sha("DEADBEEF"),
            "the console's regex is lower-case only"
        );
        assert!(!is_hex_sha("main"));
        assert!(!is_hex_sha(""));
    }
}
