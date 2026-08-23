//! Webhook intake adapters (real-world-scenarios plan, Scenario A item 2 and
//! Scenario C item 1): third-party webhook shapes in, engine-core types out.
//!
//! - `/v1/adapters/railway`: Railway's deploy-webhook payload → `DeployRecord`,
//!   only for a SUCCESS/deployed status (a build that failed or is still in
//!   progress is not a deploy — recording it as one would fabricate an
//!   attribution anchor that never actually shipped).
//! - `/v1/adapters/agentation` and `/v1/adapters/linear`: generic webhook
//!   intake → `ReportedIssue`, kind `reported`. These do NOT enter the
//!   failure/repair pipeline — see `Ingested::Reported`.
//!
//! Both families are deliberately liberal about the exact payload shape
//! (real webhook payloads carry many more fields than any one of these
//! products documents, and shapes drift): a handful of well-known field
//! paths are tried in order, unrecognized fields are ignored rather than
//! rejected, and every fallback is an honest default ("railway deploy", not
//! a fabricated commit message) — never a guess dressed up as data.
//!
//! ## Two ids on a reported issue, not one
//!
//! A `ReportedIssue` gets a fresh ULID as its `id` AND keeps the provider's
//! own identifier in `external_id`. Both are load-bearing and neither can do
//! the other's job.
//!
//! The ULID is Drums' name for the intake: the record, the CLI events and the
//! console all key on it, and it exists even for a payload that arrived with
//! no id of its own. The provider's id is the ADDRESS of the thing a repair
//! has to answer on — a Linear comment is posted against Linear's issue UUID,
//! and a ULID names nothing on Linear's side, so a write-back that only had
//! the ULID went to no issue at all.
//!
//! Overwriting `id` with the provider's identifier was the obvious fix and is
//! the wrong one: it would rename every intake the rest of the engine already
//! refers to, leave an Agentation report that carries no id unnameable, and
//! make `id` collide the day two trackers hand us the same string. So the
//! provider's identity is carried beside ours, and it is `None` — never
//! guessed, never borrowed from the ULID — when the payload had none.
//!
//! ## Agentation reads its own shape; Linear stays on the generic path
//!
//! An Agentation annotation (their AFS 1.1 schema) has none of the fields the
//! generic title extractor looks for — no `title`, `name`, `subject`,
//! `data.title` or `issue.title`, because the thing a person produced is a
//! `comment` pinned to an `elementPath`. So every annotation used to land in
//! the record titled with the literal fallback "agentation reported issue",
//! and every field that made it actionable was dropped on the floor.
//!
//! `/v1/adapters/agentation` therefore reads the annotation FIRST and falls
//! back to the generic extractors only where the annotation genuinely says
//! nothing (see [`ReportedOverrides`]). Linear's route passes no overrides at
//! all, so nothing about that path moved.
//!
//! The locating context this now carries (element, elementPath,
//! reactComponents, …) makes ATTRIBUTION better aimed — it says where on the
//! page the person was pointing. It is emphatically NOT evidence. A reported
//! issue carries no replayable request (`engine_core::Intake::Reported`), so
//! reproduction is still skipped and `engine-check`'s "does this resolve what
//! they reported" claim is still `unresolved`: a precise pointer at a button
//! is not a proof that anything about that button was checked.

use axum::extract::State;
use axum::http::StatusCode;
use axum::{Json, Router};
use axum::routing::post;
use engine_core::{Claim, DeployRecord, Provenance, ReportedIssue};
use serde_json::Value;

use crate::{now_ms, Ingested, IngestState};

const EXCERPT_MAX_CHARS: usize = 500;
const EXCERPT_TRUNCATED_MARKER: &str = "…";

/// Longer than any identifier any tracker issues (a Linear UUID is 36 chars,
/// an `ENG-123` is single digits), and short enough that a webhook sender
/// putting a paragraph in an `id` field is recognised as not having sent one.
const EXTERNAL_ID_MAX_CHARS: usize = 200;

/// Budget for a title composed from a person's own sentence. One `drums watch`
/// row's worth: the full comment survives in `body_excerpt` and verbatim in
/// `payload`, so cutting here loses nothing that isn't recorded twice over.
const TITLE_MAX_CHARS: usize = 120;

pub fn router(state: IngestState) -> Router {
    Router::new()
        .route("/v1/adapters/railway", post(post_railway))
        .route("/v1/adapters/agentation", post(post_agentation))
        .route("/v1/adapters/linear", post(post_linear))
        .with_state(state)
}

// -- shared payload navigation -------------------------------------------------

/// Walks `path` (a sequence of object keys) into `v`, stopping at the first
/// missing key. Never panics, never guesses past a shape that doesn't match.
fn get_path<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cur = v;
    for seg in path {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

/// Tries each candidate path in order, returning the first non-empty string
/// found. This is the "liberal extraction" primitive every field below is
/// built from — one webhook sender's `commitHash` is another's
/// `deployment.meta.commitHash`.
fn first_str(v: &Value, paths: &[&[&str]]) -> Option<String> {
    for path in paths {
        if let Some(s) = get_path(v, path).and_then(|x| x.as_str()) {
            if !s.trim().is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

// -- Railway deploy adapter -------------------------------------------------

fn railway_status(v: &Value) -> Option<String> {
    first_str(v, &[&["deployment", "status"], &["status"]])
}

/// Railway's own deploy statuses include BUILDING, DEPLOYING, SUCCESS,
/// FAILED, CRASHED, REMOVED, SKIPPED, QUEUED — only a completed, live deploy
/// counts. "DEPLOYED" is accepted too since not every sender uses Railway's
/// exact vocabulary (case-insensitive, matching this adapter's liberal
/// posture).
fn railway_is_success(status: &str) -> bool {
    matches!(status.to_uppercase().as_str(), "SUCCESS" | "DEPLOYED")
}

fn railway_sha(v: &Value) -> Option<String> {
    first_str(
        v,
        &[
            &["deployment", "meta", "commitHash"],
            &["meta", "commitHash"],
            &["commitHash"],
            &["commit", "sha"],
            &["commitSha"],
            &["commit_sha"],
            &["sha"],
            // No commit sha anywhere: the deployment id is still a real,
            // stable identifier for THIS deploy (spec: "commit sha/id"),
            // better than refusing a deploy Railway itself says succeeded.
            &["deployment", "id"],
            &["id"],
        ],
    )
}

fn railway_description(v: &Value) -> String {
    first_str(
        v,
        &[
            &["deployment", "meta", "commitMessage"],
            &["meta", "commitMessage"],
            &["commitMessage"],
            &["commit", "message"],
        ],
    )
    .unwrap_or_else(|| "railway deploy".to_string())
}

fn railway_author(v: &Value) -> String {
    first_str(
        v,
        &[
            &["deployment", "meta", "commitAuthor"],
            &["meta", "commitAuthor"],
            &["commitAuthor"],
            &["commit", "author"],
            &["deployment", "creator", "name"],
            &["creator", "name"],
        ],
    )
    .unwrap_or_else(|| "railway deploy".to_string())
}

/// `received_at_ms` is the ingest receipt time — used as `deployed_at_ms`
/// only because Railway's webhook payload shape does not reliably carry a
/// parseable deploy timestamp this adapter can trust; it is an honest "when
/// Drums learned about this," not a fabricated deploy time.
fn railway_deploy_from_payload(v: &Value, received_at_ms: u64) -> Option<DeployRecord> {
    let status = railway_status(v)?;
    if !railway_is_success(&status) {
        return None;
    }
    let sha = railway_sha(v)?;
    Some(DeployRecord { sha, description: railway_description(v), author: railway_author(v), deployed_at_ms: received_at_ms })
}

async fn post_railway(State(s): State<IngestState>, Json(v): Json<Value>) -> StatusCode {
    let Ok(received_at_ms) = now_ms() else {
        return StatusCode::INTERNAL_SERVER_ERROR;
    };
    let Some(deploy) = railway_deploy_from_payload(&v, received_at_ms) else {
        // A non-success status (still building, failed, crashed, ...) or a
        // success with no identifiable sha/id is a real, well-formed
        // webhook — just not a deploy to attribute future failures to.
        // Acknowledged, not recorded, not forwarded.
        return StatusCode::ACCEPTED;
    };
    if s.append("deploy", deploy.clone()).await.is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    if s.tx.send(Ingested::Deploy(deploy)).is_err() {
        tracing::error!(kind = "railway_deploy", "ingest channel receiver dropped; item not forwarded");
    }
    StatusCode::ACCEPTED
}

// -- Agentation / Linear "reported" adapters -------------------------------------------------

fn reported_title(v: &Value, source: &str) -> String {
    first_str(
        v,
        &[
            &["title"],
            &["name"],
            &["subject"],
            &["data", "title"],
            &["issue", "title"],
        ],
    )
    .unwrap_or_else(|| format!("{source} reported issue"))
}

fn reported_body(v: &Value) -> String {
    let raw = first_str(
        v,
        &[
            &["body"],
            &["description"],
            &["note"],
            &["comment"],
            &["message"],
            &["data", "body"],
            &["data", "description"],
            &["issue", "description"],
        ],
    )
    .unwrap_or_default();
    truncate_excerpt(&raw)
}

fn truncate_excerpt(s: &str) -> String {
    truncate_to(s, EXCERPT_MAX_CHARS)
}

/// Char-bounded truncation with a visible marker. Counts CHARS, never bytes:
/// slicing a multi-byte comment (an emoji in a UI complaint is not rare) at a
/// byte offset panics.
fn truncate_to(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}{EXCERPT_TRUNCATED_MARKER}")
    } else {
        s.to_string()
    }
}

fn reported_url(v: &Value) -> Option<String> {
    first_str(v, &[&["url"], &["html_url"], &["link"], &["permalink"], &["data", "url"], &["issue", "url"]])
}

/// The provider's own id for the issue this webhook is about.
///
/// The ORDER is the whole content of this function, and it is not the usual
/// specific-shape-first-then-fallback ordering the fields above use. Linear
/// sends one webhook per entity, so a `Comment` webhook's `data.id` is the id
/// of the COMMENT and the issue is one level in, at `data.issue.id` or
/// `data.issueId`. A list that tried `data.id` first would return a comment id
/// for every reply on an issue, and a comment id used as an issue id is a
/// write-back addressed to nothing — the same silent nowhere this whole field
/// exists to fix, just harder to spot because the value looks like a real
/// Linear UUID. So every path that names the issue EXPLICITLY is exhausted
/// before any path that merely names "the thing this payload is about".
///
/// Deliberately not keyed off Linear's `type` discriminator. Agentation sends
/// no such field, shapes drift, and this adapter's posture is to recognise
/// what it knows rather than to demand a envelope it was promised.
fn reported_external_id(v: &Value) -> Option<String> {
    bounded_identifier(first_str(
        v,
        &[
            &["data", "issue", "id"],
            &["data", "issueId"],
            &["data", "issue_id"],
            &["issue", "id"],
            &["issueId"],
            &["issue_id"],
            &["data", "id"],
            &["id"],
        ],
    ))
}

/// The identifier a person recognises — Linear's `ENG-123`.
///
/// Same issue-first ordering and the same reason. This one is never used to
/// address a write (see `ReportedIssue::external_identifier`), so being wrong
/// here costs a confusing line rather than a lost comment — but a confusing
/// line about which issue was repaired is not much better.
fn reported_external_identifier(v: &Value) -> Option<String> {
    bounded_identifier(first_str(
        v,
        &[
            &["data", "issue", "identifier"],
            &["issue", "identifier"],
            &["data", "identifier"],
            &["identifier"],
        ],
    ))
}

/// Trims a candidate identifier, and DROPS it rather than truncating it.
///
/// Every other field in this file may be shortened for display. This one
/// decides where a write goes, and a truncated identifier is not a shorter
/// identifier — it is a different one, addressing nothing or, worse,
/// addressing somebody else's issue. The only honest answer to "this does not
/// look like an id" is `None`, which downstream reads as "there is nowhere to
/// answer" and stays quiet.
///
/// The trim matters for the same reason: a tracker rejects `" abc "` as a
/// missing issue, and it would be rejected at the moment of the write-back,
/// far away from the payload that caused it.
fn bounded_identifier(candidate: Option<String>) -> Option<String> {
    let trimmed = candidate?.trim().to_string();
    if trimmed.is_empty() || trimmed.chars().count() > EXTERNAL_ID_MAX_CHARS {
        return None;
    }
    Some(trimmed)
}

// -- Agentation's own annotation shape -------------------------------------------------

/// Budget for the COMPOSED Agentation body — the person's comments plus the
/// labeled locating lines beneath each one. Deliberately larger than
/// `EXCERPT_MAX_CHARS` (which stays the budget for the generic path's single
/// free-text field, unchanged):
///
/// - this string is the ONLY diagnostic input the reported-issue repair hands
///   the agent — `engine-cli::repair_reported` copies it into
///   `engine_check::IssueTask::body`, which lands in the synthetic failure's
///   `stack` and is re-truncated there at 4000 chars. A 500-char cap here
///   silently threw away the elementPath of every note after the first;
/// - unlike a raw excerpt it is composed from individually bounded parts
///   (`AGENTATION_COMMENT_MAX_CHARS`, `AGENTATION_CONTEXT_VALUE_MAX_CHARS`,
///   `AGENTATION_MAX_RENDERED_ANNOTATIONS`), so no single webhook field can
///   spend the whole budget;
/// - it costs the record nothing it wasn't already storing: the entire
///   envelope is persisted verbatim (redacted) in `payload` regardless.
const AGENTATION_BODY_MAX_CHARS: usize = 2000;

/// Per-annotation comment budget inside the composed body, so one essay in a
/// `submit` of five cannot starve the other four out of the excerpt.
const AGENTATION_COMMENT_MAX_CHARS: usize = 400;

/// Per-line budget for a locating value. An `elementPath` on a deep DOM is
/// long but bounded; a sender that puts a document in `nearbyText` gets cut.
const AGENTATION_CONTEXT_VALUE_MAX_CHARS: usize = 200;

/// How many annotations of a `submit` are rendered into the body in full. The
/// overflow is named and counted rather than dropped in silence — see
/// `agentation_body`.
const AGENTATION_MAX_RENDERED_ANNOTATIONS: usize = 5;

/// The annotation fields carried into `body_excerpt`, labeled with
/// Agentation's OWN key names so a line in the record traces straight back to
/// the field it came from with no translation table in between. All are
/// `string` in AFS 1.1.
///
/// `element`/`elementPath`/`reactComponents` are the locating three: what was
/// clicked, where it sits in the DOM, and which React component rendered it —
/// the last is what turns "a button on /checkout" into a file to open.
/// `selectedText`/`nearbyText` are the person's own surroundings, which are
/// usually literal strings in the source and therefore greppable.
/// `intent`/`severity` are their classification of their own report, which
/// changes what a repair should even attempt ("question" is not "fix").
///
/// Nothing here is a claim. It is where the reporter pointed, recorded as
/// such — see the module header on why locating context never upgrades a
/// reported issue's provenance.
const AGENTATION_CONTEXT_FIELDS: &[&str] =
    &["element", "elementPath", "reactComponents", "selectedText", "nearbyText", "intent", "severity"];

/// Event-name verbs on an `annotation`/`annotations` subject that mean the
/// delivery is NOT a new report.
///
/// Both vocabularies are listed on purpose: Agentation's webhook docs name
/// `annotation.update` / `annotation.delete` / `annotations.clear`, and its
/// streaming (SSE) envelope names `annotation.updated` / `annotation.deleted`.
/// We control neither and they disagree, so the verb is matched with the
/// subject stripped and both tenses accepted rather than pinned to one
/// spelling that a sender is free to not use.
const AGENTATION_NON_CREATING_VERBS: &[&str] =
    &["update", "updated", "delete", "deleted", "remove", "removed", "clear", "cleared"];

/// What a delivery to `/v1/adapters/agentation` means.
enum AgentationDelivery<'a> {
    /// A well-formed delivery that is not a new report: an edit, a deletion, a
    /// clear-all. Acknowledged, recorded nowhere, forwarded nowhere — the same
    /// posture `post_railway` takes for a non-success deploy. Carries the event
    /// name for the log line, so an operator wiring a webhook can see WHICH
    /// event was skipped rather than wondering why nothing appeared.
    Ignored(String),
    /// The annotations this delivery is reporting. Possibly EMPTY, which means
    /// something arrived that is not annotation-shaped at all; that falls
    /// through to the generic extractors rather than being refused, matching
    /// this module's liberal posture.
    Report(Vec<&'a Value>),
}

/// Classifies a delivery by its event name BEFORE anything reads its
/// annotations, which is the whole reason this function exists as a separate
/// step: `annotation.delete` and `annotations.clear` both arrive CARRYING the
/// annotation(s) they are removing. A reader that keyed off "is there an
/// annotation in here" would file a fresh report — and spend a repair agent —
/// every time somebody deleted a note.
///
/// An event name that is neither a known creation nor a known non-creation is
/// NOT ignored. Dropping a delivery is silent data loss, and the next spelling
/// of "created" is exactly the kind of thing that appears without warning; an
/// unrecognized event whose payload has nothing annotation-shaped in it still
/// reaches the record through the generic path, where a human can see it.
fn agentation_delivery(v: &Value) -> AgentationDelivery<'_> {
    // `event` is the webhook envelope's discriminator, `type` the SSE one.
    let event = first_str(v, &[&["event"], &["type"]]).unwrap_or_default();
    let lowered = event.trim().to_ascii_lowercase();
    if let Some((subject, verb)) = lowered.split_once('.') {
        if matches!(subject, "annotation" | "annotations") && AGENTATION_NON_CREATING_VERBS.contains(&verb) {
            return AgentationDelivery::Ignored(event);
        }
    }
    AgentationDelivery::Report(agentation_annotations(v))
}

/// Finds the annotation objects in a delivery, across the three envelopes seen
/// in the wild plus the naked one:
///
/// - `{event: "annotation.add", annotation: {...}}` — one note, the webhook
///   shape (also `annotation.created`, its SSE spelling);
/// - `{type: "annotation.created", payload: {...}}` — the SSE envelope, which
///   names the same object `payload` instead of `annotation`;
/// - `{event: "submit", annotations: [...], output}` — the Send button, several
///   notes at once;
/// - a bare annotation posted with no envelope at all, which is what a
///   hand-rolled integration tends to send.
///
/// Order matters only in that the single-annotation keys are checked before
/// the array: no envelope carries both, and preferring the specific key keeps
/// a future envelope that carries an empty `annotations: []` alongside a real
/// `annotation` from resolving to nothing.
fn agentation_annotations(v: &Value) -> Vec<&Value> {
    for key in ["annotation", "payload"] {
        if let Some(a) = v.get(key).filter(|a| a.is_object()) {
            return vec![a];
        }
    }
    if let Some(items) = v.get("annotations").and_then(|a| a.as_array()) {
        return items.iter().filter(|i| i.is_object()).collect();
    }
    // A naked annotation is recognised by the two fields AFS 1.1 makes
    // required and nothing else does — never by "it is an object", which
    // would make every unrecognized payload an annotation.
    if v.get("comment").is_some() || v.get("elementPath").is_some() {
        return vec![v];
    }
    Vec::new()
}

/// The person's own words on this annotation, if they typed any.
fn annotation_comment(a: &Value) -> Option<&str> {
    a.get("comment").and_then(|c| c.as_str()).map(str::trim).filter(|c| !c.is_empty())
}

/// The first line with anything on it, trimmed. A comment is a textarea's
/// contents: people press Enter, and a title is one line by definition (the
/// `drums watch` row renderer strips control chars, so a multi-line title
/// would silently run together rather than fail loudly).
fn first_line(s: &str) -> &str {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}

/// Collapses every whitespace run to a single space.
///
/// Not cosmetic: each context line is `label: value`, and a value containing a
/// newline would otherwise swallow the labels beneath it into what looks like
/// its own continuation. This is a legibility guarantee, not a trust boundary
/// — the sender controls every one of these fields anyway, so a forged
/// `elementPath:` line inside a comment tells the agent nothing the real
/// `elementPath` field could not have said.
fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One labeled context line's value: a string, or an array of strings joined
/// (a sender that models `reactComponents` as a component list rather than
/// AFS 1.1's single string). Anything else — an object, a number, a bool — is
/// SKIPPED rather than stringified: JSON re-printed into a prose blob is noise
/// the agent has to re-parse, and the whole annotation is in `payload` intact
/// for anyone who needs the parts this doesn't understand.
fn context_value(v: &Value) -> Option<String> {
    let raw = match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().filter_map(|i| i.as_str()).collect::<Vec<_>>().join(" > "),
        _ => return None,
    };
    let collapsed = collapse_whitespace(&raw);
    if collapsed.is_empty() {
        None
    } else {
        Some(truncate_to(&collapsed, AGENTATION_CONTEXT_VALUE_MAX_CHARS))
    }
}

/// The page the report is about: the first annotation that names one, else the
/// envelope's own `url`.
///
/// Resolved here rather than left to `reported_url` (which would find the same
/// envelope `url`) because `agentation_body` needs to know WHICH url won, so
/// it can print the ones that lost — see the per-annotation url line there.
fn agentation_url(annotations: &[&Value], envelope: &Value) -> Option<String> {
    annotations
        .iter()
        .copied()
        .find_map(|a| first_str(a, &[&["url"]]))
        .or_else(|| first_str(envelope, &[&["url"]]))
}

/// The title: the first line of the first comment somebody actually typed.
///
/// `None` — falling through to the generic path and its honest
/// "agentation reported issue" — only when there is genuinely no comment and
/// no `submit` output anywhere. A blank title is never invented from the
/// element path: `button.checkout-submit` is not what anyone reported.
fn agentation_title(annotations: &[&Value], envelope: &Value) -> Option<String> {
    let comments: Vec<&str> = annotations.iter().copied().filter_map(annotation_comment).collect();
    let head = match comments.first() {
        Some(first) => first_line(first),
        // `submit` also carries Agentation's own formatted `output` blob. It is
        // still the reporter's text, so it beats the source-name fallback — but
        // only once no annotation carried a comment of its own.
        None => first_line(envelope.get("output").and_then(|o| o.as_str()).unwrap_or_default()),
    };
    if head.is_empty() {
        return None;
    }
    let mut title = truncate_to(head, TITLE_MAX_CHARS);
    // A `submit` is ONE report about several notes (see `post_agentation`), so
    // the title has to admit that the other notes exist — otherwise the record
    // and the watch row name one complaint for work that answers three.
    if comments.len() > 1 {
        title.push_str(&format!(" (+{} more)", comments.len() - 1));
    }
    Some(title)
}

/// The body: each annotation's comment followed by its locating context, as
/// `label: value` lines using Agentation's own field names.
///
/// Composed rather than echoed, because the raw JSON is already in `payload`
/// and what this field feeds is a repair agent reading prose. Everything in it
/// is the reporter's, quoted — nothing here is Drums characterising what the
/// reporter meant, and nothing here is a check that ran.
fn agentation_body(annotations: &[&Value], envelope: &Value, report_url: Option<&str>) -> Option<String> {
    let numbered = annotations.len() > 1;
    let mut blocks: Vec<String> = Vec::new();
    for (i, a) in annotations.iter().take(AGENTATION_MAX_RENDERED_ANNOTATIONS).enumerate() {
        let mut lines: Vec<String> = Vec::new();
        if let Some(comment) = annotation_comment(a) {
            let text = truncate_to(comment, AGENTATION_COMMENT_MAX_CHARS);
            lines.push(if numbered { format!("[{}] {text}", i + 1) } else { text });
        }
        for key in AGENTATION_CONTEXT_FIELDS {
            if let Some(value) = a.get(*key).and_then(context_value) {
                lines.push(format!("{key}: {value}"));
            }
        }
        // A `submit` can span pages — the person navigated, annotated, then
        // sent. `ReportedIssue::url` can only name one of those pages, so any
        // annotation that disagrees with it says so on its own line; dropping
        // it would silently relocate that note onto a page it isn't about.
        if let Some(url) = first_str(a, &[&["url"]]) {
            // Compared raw, printed bounded: the comparison has to be against
            // what `agentation_url` actually chose, but an unbounded url on the
            // first note would otherwise spend the whole body budget.
            if Some(url.as_str()) != report_url {
                lines.push(format!("url: {}", truncate_to(&collapse_whitespace(&url), AGENTATION_CONTEXT_VALUE_MAX_CHARS)));
            }
        }
        if !lines.is_empty() {
            blocks.push(lines.join("\n"));
        }
    }
    if blocks.is_empty() {
        // Nothing readable in any annotation: `submit`'s `output` blob is the
        // last reporter-supplied text there is. Absent that, `None` — the
        // generic path then gets its turn.
        let output = envelope.get("output").and_then(|o| o.as_str()).unwrap_or_default().trim();
        return (!output.is_empty()).then(|| truncate_to(output, AGENTATION_BODY_MAX_CHARS));
    }
    // The overflow is COUNTED, not dropped in silence: a body that just stops
    // after five notes reads as the whole report, and the sixth complaint
    // would look like something nobody sent.
    if annotations.len() > AGENTATION_MAX_RENDERED_ANNOTATIONS {
        blocks.push(format!(
            "(+{} more annotations in this submit — all of them are in the recorded payload)",
            annotations.len() - AGENTATION_MAX_RENDERED_ANNOTATIONS
        ));
    }
    Some(truncate_to(&blocks.join("\n\n"), AGENTATION_BODY_MAX_CHARS))
}

/// The annotation's own id — and only when this report is about exactly ONE
/// annotation.
///
/// A `submit` of three notes is one report about three things, and no one of
/// their ids names it. Borrowing the first would produce an identifier that
/// addresses a third of the report, which is `bounded_identifier`'s rule in a
/// different costume: the honest answer to "which one is it" is `None` when it
/// is not one.
fn agentation_external_id(annotations: &[&Value]) -> Option<String> {
    let [only] = annotations else { return None };
    bounded_identifier(first_str(only, &[&["id"]]))
}

/// Everything a source-specific reader resolved from a shape the generic
/// extractors cannot see. Every slot is `Option` and every `None` means
/// exactly one thing — "fall back to the generic path" — which is what keeps
/// the Linear route (it passes all-`None`) doing precisely what it did before.
#[derive(Default)]
struct ReportedOverrides {
    title: Option<String>,
    body_excerpt: Option<String>,
    url: Option<String>,
    external_id: Option<String>,
}

fn agentation_overrides(annotations: &[&Value], envelope: &Value) -> ReportedOverrides {
    let url = agentation_url(annotations, envelope);
    ReportedOverrides {
        title: agentation_title(annotations, envelope),
        body_excerpt: agentation_body(annotations, envelope, url.as_deref()),
        external_id: agentation_external_id(annotations),
        url,
    }
}

/// Redacts the copy of `payload` that goes into the record — same discipline
/// `engine-ingest::post_event` applies to `ErrorEvent.request.body` (spec:
/// "Redaction applies to reported payload bodies same as events"), composed
/// from the same `engine_record` primitives that discipline already uses,
/// applied for three different leak shapes:
///
/// 1. Object-key-based masking (`redact_body`'s JSON branch, via a
///    round-trip through a string): a sensitive key ANYWHERE in the payload
///    (`{"token": "..."}`) is masked regardless of nesting depth.
/// 2. Every remaining string leaf is then run back through `redact_body` as
///    a plain (non-JSON) string: a webhook sender that puts kv-shaped
///    secrets inside a free-text field (`{"body": "card=4242..."}`, exactly
///    the shape a reported-issue "note"/"description" field takes) would
///    otherwise survive step 1, which only inspects object keys, not the
///    contents of string values.
/// 3. Every string leaf is also run through `redact_query_string`: `payload`
///    is a verbatim echo of the whole posted JSON, so a field like `url`
///    (`https://host/path?api_key=...`) is duplicated into a leaf here too —
///    and a full URL's `https:` colon fails step 2's form-shaped-key check,
///    so a query-string-shaped secret embedded in an otherwise free-text
///    leaf would silently survive both prior passes without this one.
///
/// A payload that somehow fails to re-serialize (should never happen for a
/// `Value` that just deserialized cleanly) is left as-is rather than losing
/// the record line entirely.
fn redact_payload_for_record(payload: &Value) -> Value {
    let Ok(raw) = serde_json::to_string(payload) else { return payload.clone() };
    let key_redacted = engine_record::redact_body(Some("application/json"), &raw, &[]);
    let mut v: Value = serde_json::from_str(&key_redacted).unwrap_or_else(|_| payload.clone());
    redact_string_leaves(&mut v);
    v
}

/// THE one place "a webhook sender can put a token in any field" is answered.
/// Every untrusted string that reaches the record — `title`, `body_excerpt`,
/// `url`, both external identifiers, and every string leaf of `payload` — goes
/// through this and nothing else, so a field added to `ReportedIssue`
/// tomorrow has one obvious thing to call rather than a choice of three.
///
/// Three passes, each covering a leak the others cannot see:
///
/// 1. `redact_body` over the WHOLE string first. Only a whole-string pass can
///    see a JSON document pasted into a free-text field (a pretty-printed blob
///    in a Linear description, say) and mask it by object key at any depth.
/// 2. `redact_body` again, PER LINE. A `key=value` secret sitting on its own
///    line inside a multi-line body is invisible to pass 1: the kv splitter
///    only breaks on `&` and `;`, so the whole blob is one segment whose "key"
///    half contains whitespace and is rejected as prose. Per line it is a
///    clean pair and gets masked.
/// 3. `redact_query_string`, also per line. A full URL's `https:` colon fails
///    the form-shaped-key check in passes 1 and 2, so `?api_key=…` embedded in
///    otherwise free text survives both — this is the same third leak shape
///    `redact_payload_for_record` documents, and until this helper existed
///    `body_excerpt` (which is where a reported URL lands) never got it.
///
/// Passes 2 and 3 are per line rather than whole-string for a second reason:
/// `redact_query_string` replaces everything from a sensitive key to the next
/// `&`, which on a whole multi-line body means one URL-with-a-token deletes
/// every line after it from the record. Line by line, the loss is bounded to
/// the line that actually carried the secret, and coverage is identical — a
/// URL cannot span a newline.
///
/// The line split normalizes `\r\n` to `\n` and drops a trailing newline in
/// the RECORD copy. That is a deliberate, visible cost: the channel copy keeps
/// the bytes as sent, and no downstream reader of the record treats trailing
/// whitespace as meaning.
fn redact_text_for_record(s: &str) -> String {
    let whole = engine_record::redact_body(None, s, &[]);
    whole.lines().map(redact_line_for_record).collect::<Vec<_>>().join("\n")
}

/// Passes 2 and 3 of [`redact_text_for_record`], plus the one this file's own
/// composition made necessary.
///
/// `agentation_body` writes `label: value` lines, and the label DEFEATS the kv
/// matcher: with `elementPath: ` in front, the key half of
/// `elementPath: token=SECRET` is `elementPath: token`, which contains a space
/// and a colon and is therefore rejected as prose — so a token planted in a
/// locating field sailed through both passes and reached the record in
/// `body_excerpt` while the identical string was masked in `payload`. Caught
/// by `agentation_route_masks_tokens_planted_in_the_newly_extracted_fields`.
///
/// So a line that looks like `<single-word-label>: <rest>` gets `rest` masked
/// on its own — the value as it arrived, with nothing of ours in front of it —
/// and the label put back. Requiring the label half to be one bare word is
/// what keeps this from firing on arbitrary prose that happens to contain a
/// colon; a line that doesn't match is left to the two general passes, exactly
/// as before.
fn redact_line_for_record(line: &str) -> String {
    let masked = mask_kv_and_query(line);
    match masked.split_once(": ") {
        Some((label, value)) if !label.is_empty() && !label.contains(char::is_whitespace) => {
            format!("{label}: {}", mask_kv_and_query(value))
        }
        _ => masked,
    }
}

/// The two general passes of [`redact_text_for_record`] — kv-shaped secrets,
/// then query-string-shaped ones — applied to a single string.
fn mask_kv_and_query(s: &str) -> String {
    let kv_masked = engine_record::redact_body(None, s, &[]);
    engine_record::redact_query_string(&kv_masked, &[])
}

/// Recursively runs every string leaf in `v` through `redact_text_for_record`
/// — see `redact_payload_for_record` for why the leaves need masking of their
/// own on top of the JSON-object-key pass above.
fn redact_string_leaves(v: &mut Value) {
    match v {
        Value::String(s) => {
            *s = redact_text_for_record(s);
        }
        Value::Object(map) => {
            for val in map.values_mut() {
                redact_string_leaves(val);
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                redact_string_leaves(item);
            }
        }
        _ => {}
    }
}

/// Builds the intake, preferring anything a source-specific reader already
/// resolved and falling back to the generic extractors field by field — not
/// all-or-nothing. An Agentation annotation that carries a comment but no id
/// gets its own title and the generic path's answer on the id (`None`), which
/// is the same answer either way but arrived at by the code that knows why.
fn build_reported_issue(source: &'static str, payload: Value, overrides: ReportedOverrides) -> ReportedIssue {
    let title = overrides.title.unwrap_or_else(|| reported_title(&payload, source));
    let body_excerpt = overrides.body_excerpt.unwrap_or_else(|| reported_body(&payload));
    let url = overrides.url.or_else(|| reported_url(&payload));
    let external_id = overrides.external_id.or_else(|| reported_external_id(&payload));
    let external_identifier = reported_external_identifier(&payload);
    ReportedIssue {
        // Ours, always minted, never the provider's — see the module header.
        id: ulid::Ulid::new().to_string(),
        source: source.to_string(),
        external_id,
        external_identifier,
        title,
        body_excerpt,
        url,
        payload,
        claim: Claim { text: format!("reported via {source} webhook"), provenance: Provenance::Observed },
    }
}

async fn handle_reported(
    s: IngestState,
    source: &'static str,
    payload: Value,
    overrides: ReportedOverrides,
) -> StatusCode {
    let issue = build_reported_issue(source, payload, overrides);

    // Redact a copy for the record only. Every text field here — including the
    // Agentation locating context, which is composed from `elementPath`,
    // `reactComponents` and the rest of an annotation and is therefore just as
    // sender-controlled as the comment above it — is lifted out of the exact
    // same untrusted webhook JSON, so all of them get the same masking `event`
    // bodies get, through the same `redact_text_for_record`. The
    // channel-forwarded issue (used for narration and for addressing the
    // write-back, never replayed) keeps the raw copy so the record isn't the
    // only place that ever saw the real value.
    //
    // Masking the two external identifiers is not belt-and-braces: a sender is
    // free to put `token=…` in a field called `id`, and those strings would
    // otherwise be the one part of the payload that reached the record
    // unmasked. The channel copy keeps them raw because that is the copy a
    // write-back is addressed with — masking there would break the write the
    // field exists to make possible.
    let mut for_record = issue.clone();
    for_record.title = redact_text_for_record(&issue.title);
    for_record.body_excerpt = redact_text_for_record(&issue.body_excerpt);
    for_record.url = issue.url.as_deref().map(redact_text_for_record);
    for_record.payload = redact_payload_for_record(&issue.payload);
    for_record.external_id = issue.external_id.as_deref().map(redact_text_for_record);
    for_record.external_identifier = issue.external_identifier.as_deref().map(redact_text_for_record);

    if s.append("reported", for_record).await.is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    if s.tx.send(Ingested::Reported(issue)).is_err() {
        tracing::error!(kind = "reported", source, "ingest channel receiver dropped; item not forwarded");
    }
    StatusCode::ACCEPTED
}

/// One webhook delivery is one report, including a `submit` carrying several
/// annotations. Splitting a submit into N reports was the alternative and it
/// is worse on three counts:
///
/// - `ReportedIssue::payload` is documented as the raw webhook payload, and N
///   reports could only carry either N copies of the whole envelope or a
///   per-annotation payload Drums synthesized — a record line claiming to be
///   what arrived when it is not;
/// - each report costs a repair: `engine-cli::repair_reported` builds a
///   worktree, runs the declared checks and opens a proposal per intake. Three
///   agents racing on three notes about the same page produce three branches
///   that conflict, where one agent seeing all three notes produces one;
/// - the person pressed Send once. The confirmed real-world integration at
///   `twentyfour26/api/app/routes/feedback.py` files one ticket per submit for
///   the same reason, so Drums' report count matches the tracker's.
///
/// Nothing is lost by merging: every annotation's comment and locating context
/// is rendered into the body (see `agentation_body`), and the full array
/// survives verbatim in `payload`.
async fn post_agentation(State(s): State<IngestState>, Json(v): Json<Value>) -> StatusCode {
    let annotations = match agentation_delivery(&v) {
        AgentationDelivery::Ignored(event) => {
            tracing::debug!(source = "agentation", %event, "acknowledged, not recorded: not a new annotation");
            return StatusCode::ACCEPTED;
        }
        AgentationDelivery::Report(annotations) => annotations,
    };
    // `annotations` borrows `v`, so everything read out of it is resolved into
    // owned strings here, before `v` itself moves into the record.
    let overrides = agentation_overrides(&annotations, &v);
    handle_reported(s, "agentation", v, overrides).await
}

async fn post_linear(State(s): State<IngestState>, Json(v): Json<Value>) -> StatusCode {
    // No overrides: Linear's webhook is exactly the shape the generic
    // extractors were written for, and reading it through Agentation's is how
    // one adapter's fix becomes another's regression.
    handle_reported(s, "linear", v, ReportedOverrides::default()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    // -- Railway: pure extraction ------------------------------------------------

    #[test]
    fn railway_success_deploy_extracts_sha_description_author() {
        let v = serde_json::json!({
            "status": "SUCCESS",
            "deployment": {
                "meta": {
                    "commitHash": "abc123def",
                    "commitMessage": "add promo code field",
                    "commitAuthor": "maya"
                }
            }
        });
        let d = railway_deploy_from_payload(&v, 1_753_000_000_000).expect("success deploy must extract");
        assert_eq!(d.sha, "abc123def");
        assert_eq!(d.description, "add promo code field");
        assert_eq!(d.author, "maya");
        assert_eq!(d.deployed_at_ms, 1_753_000_000_000);
    }

    #[test]
    fn railway_top_level_shape_also_extracts() {
        // A liberal shape: not everyone nests under `deployment`.
        let v = serde_json::json!({"status": "SUCCESS", "commitSha": "deadbeef", "commitMessage": "fix bug", "commitAuthor": "lee"});
        let d = railway_deploy_from_payload(&v, 1).expect("must extract");
        assert_eq!(d.sha, "deadbeef");
        assert_eq!(d.description, "fix bug");
        assert_eq!(d.author, "lee");
    }

    #[test]
    fn railway_deployed_status_case_insensitive_also_counts_as_success() {
        let v = serde_json::json!({"status": "deployed", "sha": "abc"});
        assert!(railway_deploy_from_payload(&v, 1).is_some());
    }

    #[test]
    fn railway_non_success_status_extracts_nothing() {
        for status in ["BUILDING", "FAILED", "CRASHED", "QUEUED"] {
            let v = serde_json::json!({"status": status, "deployment": {"meta": {"commitHash": "abc"}}});
            assert!(railway_deploy_from_payload(&v, 1).is_none(), "status {status} must not become a deploy");
        }
    }

    #[test]
    fn railway_missing_sha_falls_back_to_deployment_id() {
        let v = serde_json::json!({"status": "SUCCESS", "deployment": {"id": "dep_123"}});
        let d = railway_deploy_from_payload(&v, 1).expect("must extract via id fallback");
        assert_eq!(d.sha, "dep_123");
    }

    #[test]
    fn railway_missing_commit_message_and_author_use_honest_fallback() {
        let v = serde_json::json!({"status": "SUCCESS", "sha": "abc123"});
        let d = railway_deploy_from_payload(&v, 1).unwrap();
        assert_eq!(d.description, "railway deploy");
        assert_eq!(d.author, "railway deploy");
    }

    #[test]
    fn railway_no_status_at_all_extracts_nothing() {
        let v = serde_json::json!({"sha": "abc123"});
        assert!(railway_deploy_from_payload(&v, 1).is_none());
    }

    #[test]
    fn railway_success_with_no_sha_anywhere_extracts_nothing() {
        let v = serde_json::json!({"status": "SUCCESS"});
        assert!(railway_deploy_from_payload(&v, 1).is_none(), "must never fabricate a sha");
    }

    // -- Railway: HTTP route ------------------------------------------------

    #[tokio::test]
    async fn railway_route_success_payload_returns_202_records_and_forwards() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let (state, mut rx) = IngestState::new(path.clone());
        let app = router(state);
        let body = serde_json::json!({
            "status": "SUCCESS",
            "deployment": {"meta": {"commitHash": "abc123def", "commitMessage": "add promo code", "commitAuthor": "maya"}}
        });
        let res = app
            .oneshot(Request::post("/v1/adapters/railway").header("content-type", "application/json").body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        let got = rx.recv().await.unwrap();
        assert!(matches!(got, Ingested::Deploy(d) if d.sha == "abc123def" && d.description == "add promo code"));
        let record = std::fs::read_to_string(&path).unwrap();
        assert!(record.contains(r#""kind":"deploy""#));
        assert!(record.contains("abc123def"));
    }

    #[tokio::test]
    async fn railway_route_non_success_status_returns_202_records_nothing() {
        // Pinned choice: a real, well-formed webhook for a deploy that
        // didn't (yet, or ever) succeed is acknowledged, not treated as
        // garbage — but it must not become a DeployRecord.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let (state, mut rx) = IngestState::new(path.clone());
        let app = router(state);
        let body = serde_json::json!({"status": "FAILED", "deployment": {"meta": {"commitHash": "abc123"}}});
        let res = app
            .oneshot(Request::post("/v1/adapters/railway").header("content-type", "application/json").body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED, "a real webhook for a non-success deploy is still acknowledged");
        assert!(!path.exists() || std::fs::read_to_string(&path).unwrap().is_empty(), "must record nothing for a non-success deploy");
        // `app.oneshot` consumes the router (and with it the sole remaining
        // `IngestState`/sender clone), so the channel closes the instant the
        // request finishes — `rx.recv()` resolves to `Ok(None)` immediately
        // rather than hanging. Either that clean close OR a timeout with
        // nothing received both mean the same thing: nothing was forwarded.
        let extra = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await;
        assert!(matches!(extra, Err(_) | Ok(None)), "must forward nothing on the channel for a non-success deploy, got {extra:?}");
    }

    #[tokio::test]
    async fn railway_route_garbage_body_is_rejected_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let (state, _rx) = IngestState::new(path.clone());
        let app = router(state);
        let res = app
            .oneshot(Request::post("/v1/adapters/railway").header("content-type", "application/json").body(Body::from("{nope")).unwrap())
            .await
            .unwrap();
        assert!(res.status().is_client_error(), "malformed JSON must be rejected: {}", res.status());
        assert!(!path.exists() || std::fs::read_to_string(&path).unwrap().is_empty());
    }

    // -- Agentation / Linear: pure extraction ------------------------------------------------

    #[test]
    fn reported_title_falls_back_honestly_when_absent() {
        let v = serde_json::json!({"element": "#submit-btn"});
        assert_eq!(reported_title(&v, "agentation"), "agentation reported issue");
    }

    #[test]
    fn reported_body_truncates_long_excerpts() {
        let long = "x".repeat(1000);
        let v = serde_json::json!({"body": long});
        let out = reported_body(&v);
        assert!(out.ends_with(EXCERPT_TRUNCATED_MARKER));
        assert!(out.chars().count() < 1000);
    }

    #[test]
    fn reported_url_is_none_when_absent() {
        let v = serde_json::json!({"title": "x"});
        assert_eq!(reported_url(&v), None);
    }

    // -- The provider's own issue identity ---------------------------------
    //
    // The bug these pin: a Linear-reported failure used to get a Drums ULID and
    // nothing else, so the comment closing the loop had no issue to go to.

    /// A real Linear issue webhook, trimmed to the fields this adapter reads.
    fn linear_issue_webhook() -> Value {
        serde_json::json!({
            "action": "create",
            "type": "Issue",
            "organizationId": "5f6a1e2c-0000-4000-8000-0000000000aa",
            "data": {
                "id": "e1f2a3b4-0000-4000-8000-000000000000",
                "identifier": "ENG-123",
                "number": 123,
                "title": "crash on submit",
                "description": "500 when clicking submit twice",
                "url": "https://linear.app/acme/issue/ENG-123/crash-on-submit",
                "team": {"id": "7c8d9e0f-0000-4000-8000-000000000001", "key": "ENG"}
            }
        })
    }

    #[test]
    fn linear_issue_webhook_yields_the_graphql_uuid_and_the_human_identifier() {
        let v = linear_issue_webhook();
        assert_eq!(reported_external_id(&v).as_deref(), Some("e1f2a3b4-0000-4000-8000-000000000000"));
        assert_eq!(reported_external_identifier(&v).as_deref(), Some("ENG-123"));
    }

    #[test]
    fn linear_comment_webhook_addresses_the_issue_not_the_comment() {
        // The ordering test. `data.id` here is the COMMENT's id, and a comment
        // id posted back as an issue id writes to nothing.
        let v = serde_json::json!({
            "action": "create",
            "type": "Comment",
            "data": {
                "id": "cccccccc-0000-4000-8000-00000000000c",
                "body": "still happening on prod",
                "issueId": "e1f2a3b4-0000-4000-8000-000000000000",
                "issue": {"id": "e1f2a3b4-0000-4000-8000-000000000000", "identifier": "ENG-123", "title": "crash on submit"}
            }
        });
        assert_eq!(
            reported_external_id(&v).as_deref(),
            Some("e1f2a3b4-0000-4000-8000-000000000000"),
            "a Comment webhook must resolve to the issue it is on, never to the comment"
        );
        assert_eq!(reported_external_identifier(&v).as_deref(), Some("ENG-123"));
    }

    #[test]
    fn linear_comment_webhook_without_a_nested_issue_still_finds_the_issue_id() {
        let v = serde_json::json!({
            "type": "Comment",
            "data": {"id": "cccccccc-0000-4000-8000-00000000000c", "issueId": "e1f2a3b4-0000-4000-8000-000000000000"}
        });
        assert_eq!(reported_external_id(&v).as_deref(), Some("e1f2a3b4-0000-4000-8000-000000000000"));
    }

    #[test]
    fn a_payload_with_no_identifier_anywhere_carries_none_rather_than_a_guess() {
        let v = serde_json::json!({"title": "button misaligned", "element": "#submit-btn"});
        assert_eq!(reported_external_id(&v), None);
        assert_eq!(reported_external_identifier(&v), None);
    }

    #[test]
    fn an_over_long_or_blank_identifier_is_dropped_rather_than_truncated() {
        // Truncating would produce a well-formed id that addresses a different
        // issue, or none. Neither is better than admitting there isn't one.
        let long = serde_json::json!({"id": "x".repeat(EXTERNAL_ID_MAX_CHARS + 1)});
        assert_eq!(reported_external_id(&long), None);

        let blank = serde_json::json!({"id": "   "});
        assert_eq!(reported_external_id(&blank), None);
    }

    #[test]
    fn a_padded_identifier_is_trimmed_so_the_write_back_is_not_rejected() {
        let v = serde_json::json!({"data": {"id": "  e1f2a3b4-0000-4000-8000-000000000000\n"}});
        assert_eq!(reported_external_id(&v).as_deref(), Some("e1f2a3b4-0000-4000-8000-000000000000"));
    }

    #[test]
    fn the_drums_id_stays_a_fresh_ulid_beside_the_providers_id() {
        let issue = build_reported_issue("linear", linear_issue_webhook(), ReportedOverrides::default());
        assert!(
            ulid::Ulid::from_string(&issue.id).is_ok(),
            "the Drums-side id must stay a ULID — the record, the events and the console all key on it: {}",
            issue.id
        );
        assert_ne!(issue.id, issue.external_id.clone().unwrap_or_default());
    }

    // -- Agentation's own shape ---------------------------------------------
    //
    // The bug these pin: every annotation was titled with the literal string
    // "agentation reported issue" and every field that made it actionable —
    // the comment, the element, the DOM path, the React component — was
    // dropped, because the payload was read through Linear's field names.

    /// A realistic `annotation.add` delivery, field-for-field from Agentation's
    /// own webhook example plus the optional AFS 1.1 context fields a real
    /// browser extension fills in.
    fn agentation_annotation_add() -> Value {
        serde_json::json!({
            "event": "annotation.add",
            "timestamp": 1_706_234_567_890u64,
            "url": "https://shop.example/checkout",
            "annotation": {
                "id": "1706234567890",
                "comment": "Promo code field is cut off on mobile\nand the price overlaps it",
                "element": "button",
                "elementPath": "body > main > form.checkout > button.submit-btn",
                "timestamp": 1_706_234_567_890u64,
                "x": 42.5,
                "y": 1180,
                "boundingBox": {"x": 12.0, "y": 1160.0, "width": 320.0, "height": 44.0},
                "reactComponents": "CheckoutForm > SubmitButton",
                "cssClasses": "btn btn-primary submit-btn",
                "nearbyText": "Apply promo code",
                "intent": "fix",
                "severity": "blocking"
            }
        })
    }

    fn overrides_for(v: &Value) -> ReportedOverrides {
        let AgentationDelivery::Report(annotations) = agentation_delivery(v) else {
            panic!("expected a reportable delivery");
        };
        agentation_overrides(&annotations, v)
    }

    #[test]
    fn agentation_titles_from_the_comment_rather_than_the_source_fallback() {
        let o = overrides_for(&agentation_annotation_add());
        assert_eq!(
            o.title.as_deref(),
            Some("Promo code field is cut off on mobile"),
            "the comment is the only thing a person actually wrote; the old title was a constant"
        );
    }

    #[test]
    fn agentation_carries_the_locating_context_into_the_body() {
        let o = overrides_for(&agentation_annotation_add());
        let body = o.body_excerpt.expect("an annotation with a comment must produce a body");
        // The person's own words, both lines, before anything Drums composed.
        assert!(body.starts_with("Promo code field is cut off on mobile\nand the price overlaps it"), "{body}");
        // The locating three: what was clicked, where it is, what rendered it.
        assert!(body.contains("element: button"), "{body}");
        assert!(body.contains("elementPath: body > main > form.checkout > button.submit-btn"), "{body}");
        assert!(body.contains("reactComponents: CheckoutForm > SubmitButton"), "{body}");
        assert!(body.contains("nearbyText: Apply promo code"), "{body}");
        assert!(body.contains("intent: fix"), "{body}");
        assert!(body.contains("severity: blocking"), "{body}");
        // Geometry is not locating context a repair agent can act on, and a
        // JSON object flattened into prose is noise it would have to re-parse.
        assert!(!body.contains("boundingBox"), "non-string fields must not be stringified into the body: {body}");
        // The page and the annotation's own id come out as their own fields.
        assert_eq!(o.url.as_deref(), Some("https://shop.example/checkout"));
        assert_eq!(o.external_id.as_deref(), Some("1706234567890"));
    }

    #[test]
    fn agentation_title_takes_the_first_line_and_bounds_it() {
        let long = "x".repeat(TITLE_MAX_CHARS + 50);
        let v = serde_json::json!({"event": "annotation.add", "annotation": {"comment": format!("\n\n{long}\nsecond line")}});
        let title = overrides_for(&v).title.unwrap();
        assert!(title.ends_with(EXCERPT_TRUNCATED_MARKER), "{title}");
        assert!(!title.contains("second line"), "a title is one line: {title}");
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS + EXCERPT_TRUNCATED_MARKER.chars().count());
    }

    #[test]
    fn an_annotation_with_no_comment_falls_back_to_the_honest_generic_title() {
        // A placement/rearrange annotation can carry no typed comment at all.
        // Naming it after its element path would put words in the reporter's
        // mouth, so the generic fallback is still the right answer.
        let v = serde_json::json!({
            "event": "annotation.add",
            "annotation": {"id": "9", "kind": "placement", "elementPath": "body > main", "element": "main"}
        });
        let o = overrides_for(&v);
        assert_eq!(o.title, None, "no comment means no title of our own");
        let issue = build_reported_issue("agentation", v, o);
        assert_eq!(issue.title, "agentation reported issue");
        assert!(issue.body_excerpt.contains("elementPath: body > main"), "the context still survives: {}", issue.body_excerpt);
    }

    #[test]
    fn the_sse_envelope_spelling_is_read_like_the_webhook_one() {
        // Agentation's streaming docs name the event `annotation.created` and
        // put the annotation under `payload`; its webhook docs say
        // `annotation.add` and `annotation`. We control neither vocabulary.
        let v = serde_json::json!({
            "type": "annotation.created",
            "sessionId": "sess_1",
            "sequence": 4,
            "payload": {"id": "7", "comment": "totals are wrong", "element": "span", "elementPath": "body > span.total"}
        });
        let o = overrides_for(&v);
        assert_eq!(o.title.as_deref(), Some("totals are wrong"));
        assert!(o.body_excerpt.unwrap().contains("elementPath: body > span.total"));
        assert_eq!(o.external_id.as_deref(), Some("7"));
    }

    #[test]
    fn non_creating_events_are_ignored_in_both_vocabularies() {
        for event in [
            "annotation.update",
            "annotation.updated",
            "annotation.delete",
            "annotation.deleted",
            "annotation.clear",
            "annotations.clear",
            "ANNOTATION.DELETE",
        ] {
            // Every one of these arrives CARRYING the annotation it is removing
            // — which is exactly why the event name is read first. Keying off
            // "is there an annotation in here" would file a fresh report, and
            // spend a repair agent, for every note somebody deleted.
            let v = serde_json::json!({
                "event": event,
                "url": "https://shop.example/checkout",
                "annotation": {"id": "1", "comment": "already dealt with", "elementPath": "body"},
                "annotations": [{"id": "1", "comment": "already dealt with", "elementPath": "body"}]
            });
            assert!(
                matches!(agentation_delivery(&v), AgentationDelivery::Ignored(_)),
                "{event} must not become a new report"
            );
        }
    }

    #[test]
    fn an_unrecognized_event_is_not_ignored_because_silent_loss_is_worse() {
        // The next spelling of "created" will arrive unannounced. Recording an
        // event we cannot interpret leaves a line a human can see; dropping it
        // loses a real report with nothing to show for it.
        let v = serde_json::json!({
            "event": "annotation.flagged",
            "annotation": {"id": "3", "comment": "still broken", "elementPath": "body > p"}
        });
        assert_eq!(overrides_for(&v).title.as_deref(), Some("still broken"));
    }

    /// A `submit`: the Send button, several notes at once. One delivery, one
    /// report — see `post_agentation` for why, and note that nothing is lost:
    /// every comment and every element path is in the body.
    fn agentation_submit() -> Value {
        serde_json::json!({
            "event": "submit",
            "url": "https://shop.example/checkout",
            "output": "## Feedback\n- 3 annotations",
            "annotations": [
                {"id": "1", "comment": "promo field is cut off", "element": "input", "elementPath": "form > input.promo"},
                {"id": "2", "comment": "price overlaps the button", "element": "span", "elementPath": "form > span.price"},
                {"id": "3", "comment": "spinner never stops", "element": "div", "elementPath": "main > div.spinner"}
            ]
        })
    }

    #[test]
    fn a_submit_is_one_report_whose_title_admits_the_other_notes() {
        let o = overrides_for(&agentation_submit());
        assert_eq!(
            o.title.as_deref(),
            Some("promo field is cut off (+2 more)"),
            "one report named after one of three complaints would misdescribe the work it triggers"
        );
    }

    #[test]
    fn a_submit_body_carries_every_annotation_numbered_with_its_own_context() {
        let body = overrides_for(&agentation_submit()).body_excerpt.unwrap();
        for (n, (comment, path)) in [
            ("promo field is cut off", "form > input.promo"),
            ("price overlaps the button", "form > span.price"),
            ("spinner never stops", "main > div.spinner"),
        ]
        .iter()
        .enumerate()
        {
            assert!(body.contains(&format!("[{}] {comment}", n + 1)), "{body}");
            assert!(body.contains(&format!("elementPath: {path}")), "{body}");
        }
    }

    #[test]
    fn a_submit_of_several_annotations_has_no_single_provider_id_and_says_so() {
        assert_eq!(
            agentation_external_id(&agentation_annotations(&agentation_submit())),
            None,
            "the first note's id names a third of this report — the honest answer is None"
        );
        // One annotation in the array is still exactly one thing, and keeps its id.
        let single = serde_json::json!({"event": "submit", "annotations": [{"id": "42", "comment": "x"}]});
        assert_eq!(agentation_external_id(&agentation_annotations(&single)).as_deref(), Some("42"));
    }

    #[test]
    fn a_submit_beyond_the_render_budget_counts_the_overflow_rather_than_dropping_it() {
        let annotations: Vec<Value> = (0..AGENTATION_MAX_RENDERED_ANNOTATIONS + 3)
            .map(|i| serde_json::json!({"id": i.to_string(), "comment": format!("note {i}")}))
            .collect();
        let v = serde_json::json!({"event": "submit", "annotations": annotations});
        let body = overrides_for(&v).body_excerpt.unwrap();
        assert!(body.contains("(+3 more annotations in this submit"), "a body that just stops reads as the whole report: {body}");
        assert!(body.contains("note 0"));
        assert!(!body.contains("note 7"));
    }

    #[test]
    fn an_annotation_on_a_different_page_than_the_report_names_its_own_page() {
        // A submit can span pages: annotate, navigate, annotate, send. The
        // single `url` field can only name one of them, and dropping the rest
        // would silently relocate a note onto a page it isn't about.
        let v = serde_json::json!({
            "event": "submit",
            "url": "https://shop.example/checkout",
            "annotations": [
                {"id": "1", "comment": "a", "url": "https://shop.example/cart"},
                {"id": "2", "comment": "b", "url": "https://shop.example/checkout"}
            ]
        });
        let o = overrides_for(&v);
        assert_eq!(o.url.as_deref(), Some("https://shop.example/cart"), "the first annotation that names a page wins");
        let body = o.body_excerpt.unwrap();
        assert!(body.contains("url: https://shop.example/checkout"), "the note on the other page must say so: {body}");
        assert_eq!(body.matches("url: ").count(), 1, "the report's own page is not repeated per note: {body}");
    }

    #[test]
    fn a_bare_annotation_posted_with_no_envelope_is_still_read_as_one() {
        let v = serde_json::json!({
            "id": "1706234567890",
            "comment": "checkout button does nothing",
            "element": "button",
            "elementPath": "body > button#buy",
            "url": "https://shop.example/checkout"
        });
        let o = overrides_for(&v);
        assert_eq!(o.title.as_deref(), Some("checkout button does nothing"));
        assert!(o.body_excerpt.unwrap().contains("elementPath: body > button#buy"));
        assert_eq!(o.external_id.as_deref(), Some("1706234567890"));
    }

    #[test]
    fn a_payload_that_is_not_annotation_shaped_leaves_every_field_to_the_generic_path() {
        // "Recognise what we know" — an unrecognized body is not refused and
        // not guessed at; it goes to the generic extractors exactly as before.
        let v = serde_json::json!({"title": "button misaligned", "note": "overlaps price on mobile"});
        let o = overrides_for(&v);
        assert!(o.title.is_none() && o.body_excerpt.is_none() && o.external_id.is_none());
        let issue = build_reported_issue("agentation", v, o);
        assert_eq!(issue.title, "button misaligned");
        assert_eq!(issue.body_excerpt, "overlaps price on mobile");
    }

    #[test]
    fn a_list_valued_react_components_field_is_joined_rather_than_skipped() {
        // AFS 1.1 types `reactComponents` as a single string; a sender that
        // models the hierarchy as a list is still saying something useful.
        let v = serde_json::json!({
            "annotation": {"comment": "x", "reactComponents": ["CheckoutForm", "SubmitButton"]}
        });
        assert!(overrides_for(&v).body_excerpt.unwrap().contains("reactComponents: CheckoutForm > SubmitButton"));
    }

    #[test]
    fn a_multi_line_context_value_cannot_swallow_the_labels_under_it() {
        let v = serde_json::json!({
            "annotation": {"comment": "x", "element": "button\nelementPath: forged", "elementPath": "body > real"}
        });
        let body = overrides_for(&v).body_excerpt.unwrap();
        assert!(body.contains("element: button elementPath: forged"), "one fact per line: {body}");
        assert!(body.contains("elementPath: body > real"), "{body}");
    }

    // -- Redaction of the fields this adapter newly extracts -----------------

    #[test]
    fn redact_text_for_record_masks_per_line_so_one_url_does_not_eat_the_body() {
        let body = "see https://shop.example/c?api_key=SECRET_777 for steps\nelementPath: body > button\ntoken=SECRET_888";
        let masked = redact_text_for_record(body);
        assert!(!masked.contains("SECRET_777"), "{masked}");
        assert!(!masked.contains("SECRET_888"), "a kv secret on its own line is invisible to a whole-blob pass: {masked}");
        assert!(masked.contains("elementPath: body > button"), "masking one line must not delete the ones after it: {masked}");
    }

    #[test]
    fn a_labeled_line_does_not_hide_its_value_from_the_kv_matcher() {
        // Regression: `elementPath: ` in front turns the key half of
        // `token=SECRET` into `elementPath: token` — whitespace and a colon —
        // which the kv matcher rejects as prose. The composed line reached the
        // record unmasked while the identical raw string in `payload` was
        // masked.
        let masked = redact_text_for_record("elementPath: token=SECRET_999\nnearbyText: card=4242424242424242");
        assert!(!masked.contains("SECRET_999"), "{masked}");
        assert!(!masked.contains("4242424242424242"), "{masked}");
        assert!(masked.starts_with("elementPath: token=[redacted]"), "the label itself survives: {masked}");
    }

    // -- Agentation / Linear: HTTP routes ------------------------------------------------

    #[tokio::test]
    async fn agentation_route_valid_payload_returns_202_records_reported_kind_and_forwards() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let (state, mut rx) = IngestState::new(path.clone());
        let app = router(state);
        let body = serde_json::json!({"title": "button misaligned", "note": "overlaps price on mobile", "page": "/checkout", "url": "https://agentation.example/i/42"});
        let res = app
            .oneshot(Request::post("/v1/adapters/agentation").header("content-type", "application/json").body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        let got = rx.recv().await.unwrap();
        let Ingested::Reported(issue) = got else { panic!("expected a Reported item") };
        assert_eq!(issue.source, "agentation");
        assert_eq!(issue.title, "button misaligned");
        assert_eq!(issue.claim.provenance, Provenance::Observed);
        assert!(issue.claim.text.contains("reported via agentation webhook"));

        let record = std::fs::read_to_string(&path).unwrap();
        assert!(record.contains(r#""kind":"reported""#));
        assert!(record.contains(r#""source":"agentation""#));
    }

    #[tokio::test]
    async fn linear_route_valid_payload_returns_202_records_reported_kind_and_forwards() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let (state, mut rx) = IngestState::new(path.clone());
        let app = router(state);
        let body = serde_json::json!({"data": {"title": "crash on submit", "description": "500 when clicking submit twice"}});
        let res = app
            .oneshot(Request::post("/v1/adapters/linear").header("content-type", "application/json").body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        let got = rx.recv().await.unwrap();
        let Ingested::Reported(issue) = got else { panic!("expected a Reported item") };
        assert_eq!(issue.source, "linear");
        assert_eq!(issue.title, "crash on submit");
        assert!(issue.claim.text.contains("reported via linear webhook"));

        let record = std::fs::read_to_string(&path).unwrap();
        assert!(record.contains(r#""kind":"reported""#));
        assert!(record.contains(r#""source":"linear""#));
    }

    /// The end-to-end version of the bug: a real Linear webhook goes in the
    /// HTTP door and the issue's own id comes out both on the channel (which
    /// is what a write-back is addressed with) and in the record line (which is
    /// what a later process replays from).
    #[tokio::test]
    async fn linear_route_round_trips_the_providers_own_issue_id_to_channel_and_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let (state, mut rx) = IngestState::new(path.clone());
        let app = router(state);
        let res = app
            .oneshot(
                Request::post("/v1/adapters/linear")
                    .header("content-type", "application/json")
                    .body(Body::from(linear_issue_webhook().to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);

        let got = rx.recv().await.unwrap();
        let Ingested::Reported(issue) = got else { panic!("expected a Reported item") };
        assert_eq!(
            issue.external_id.as_deref(),
            Some("e1f2a3b4-0000-4000-8000-000000000000"),
            "without this the comment closing the loop has nowhere to go"
        );
        assert_eq!(issue.external_identifier.as_deref(), Some("ENG-123"));
        assert_ne!(issue.id, "e1f2a3b4-0000-4000-8000-000000000000", "the Drums-side id stays ours");

        let line = std::fs::read_to_string(&path).unwrap();
        let recorded: ReportedIssue =
            serde_json::from_str(line.trim()).expect("the record line must deserialize back into a ReportedIssue");
        assert_eq!(recorded.external_id.as_deref(), Some("e1f2a3b4-0000-4000-8000-000000000000"));
        assert_eq!(recorded.external_identifier.as_deref(), Some("ENG-123"));
        assert_eq!(recorded.id, issue.id);
    }

    #[tokio::test]
    async fn reported_route_redacts_sensitive_payload_fields_in_the_record_but_not_on_the_channel() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let (state, mut rx) = IngestState::new(path.clone());
        let app = router(state);
        let body = serde_json::json!({
            "title": "token=SECRET_IN_TITLE_999",
            "body": "card=4242424242424242",
            "extra": {"token": "SECRET123"},
            "url": "https://agentation.example/i/42?api_key=SECRET_IN_URL_777",
            // A sender is free to put anything in a field named `id`, and this
            // one is read straight out of the payload into `external_id`.
            "id": "token=SECRET_IN_ID_888"
        });
        let res = app
            .oneshot(Request::post("/v1/adapters/agentation").header("content-type", "application/json").body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);

        let got = rx.recv().await.unwrap();
        let Ingested::Reported(issue) = got else { panic!("expected a Reported item") };
        assert_eq!(issue.payload["extra"]["token"], "SECRET123", "channel-forwarded payload keeps the raw value");
        assert_eq!(issue.title, "token=SECRET_IN_TITLE_999", "channel-forwarded title keeps the raw value");
        assert_eq!(
            issue.url.as_deref(),
            Some("https://agentation.example/i/42?api_key=SECRET_IN_URL_777"),
            "channel-forwarded url keeps the raw value"
        );
        assert_eq!(
            issue.external_id.as_deref(),
            Some("token=SECRET_IN_ID_888"),
            "channel-forwarded external id keeps the raw value — it is what a write-back is addressed with"
        );

        let record = std::fs::read_to_string(&path).unwrap();
        assert!(!record.contains("SECRET123"), "record must never contain the raw token: {record}");
        assert!(!record.contains("4242424242424242"), "record must never contain the raw card number: {record}");
        assert!(!record.contains("SECRET_IN_TITLE_999"), "record must never contain the raw title secret: {record}");
        assert!(!record.contains("SECRET_IN_URL_777"), "record must never contain the raw url secret: {record}");
        assert!(!record.contains("SECRET_IN_ID_888"), "record must never contain the raw id secret: {record}");
        assert!(record.contains("[redacted]"), "record must carry the redaction marker: {record}");
    }

    /// End to end: a real `annotation.add` goes in the HTTP door and comes out
    /// of the record as something a person — and the repair agent that reads
    /// `body_excerpt` — can act on.
    #[tokio::test]
    async fn agentation_route_records_the_comment_as_the_title_and_keeps_the_locating_context() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let (state, mut rx) = IngestState::new(path.clone());
        let app = router(state);
        let res = app
            .oneshot(
                Request::post("/v1/adapters/agentation")
                    .header("content-type", "application/json")
                    .body(Body::from(agentation_annotation_add().to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);

        let Ingested::Reported(issue) = rx.recv().await.unwrap() else { panic!("expected a Reported item") };
        assert_eq!(issue.title, "Promo code field is cut off on mobile");
        assert!(issue.body_excerpt.contains("elementPath: body > main > form.checkout > button.submit-btn"));
        assert!(issue.body_excerpt.contains("reactComponents: CheckoutForm > SubmitButton"));
        assert_eq!(issue.url.as_deref(), Some("https://shop.example/checkout"));
        assert_eq!(issue.external_id.as_deref(), Some("1706234567890"));
        // Locating context does not upgrade anything: the intake is still one
        // `Observed` claim that a human said so. Nothing here was reproduced.
        assert_eq!(issue.claim.provenance, Provenance::Observed);
        assert_eq!(issue.claim.text, "reported via agentation webhook");

        let line = std::fs::read_to_string(&path).unwrap();
        let recorded: ReportedIssue = serde_json::from_str(line.trim()).expect("the record line must load back");
        assert_eq!(recorded.title, "Promo code field is cut off on mobile");
        assert!(recorded.body_excerpt.contains("reactComponents: CheckoutForm > SubmitButton"));
    }

    #[tokio::test]
    async fn agentation_route_acknowledges_a_delete_without_recording_or_forwarding_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let (state, mut rx) = IngestState::new(path.clone());
        let app = router(state);
        let body = serde_json::json!({
            "event": "annotation.delete",
            "url": "https://shop.example/checkout",
            "annotation": {"id": "1706234567890", "comment": "never mind, my mistake", "elementPath": "body"}
        });
        let res = app
            .oneshot(Request::post("/v1/adapters/agentation").header("content-type", "application/json").body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED, "a real webhook for a deletion is still acknowledged");
        assert!(!path.exists() || std::fs::read_to_string(&path).unwrap().is_empty(), "a deletion must never become a report");
        // Same close-or-timeout reasoning as the Railway non-success test above.
        let extra = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await;
        assert!(matches!(extra, Err(_) | Ok(None)), "a deletion must forward nothing, got {extra:?}");
    }

    /// The redaction pin for everything this adapter newly reads. A sender can
    /// put a token in ANY field, and the fields that make an annotation
    /// actionable — `elementPath`, `nearbyText`, a per-annotation `url` — are
    /// no more trustworthy than the comment above them. All of them now reach
    /// the record through `body_excerpt`, so all of them must be masked there,
    /// while the channel copy stays raw exactly as it always did.
    #[tokio::test]
    async fn agentation_route_masks_tokens_planted_in_the_newly_extracted_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let (state, mut rx) = IngestState::new(path.clone());
        let app = router(state);
        let body = serde_json::json!({
            "event": "submit",
            "url": "https://shop.example/checkout",
            "annotations": [
                {
                    "id": "1",
                    // A query-string secret inside free text: the shape that
                    // survived `body_excerpt`'s old single-pass masking,
                    // because a full URL's `https:` colon fails the
                    // form-shaped-key check.
                    "comment": "repro at https://shop.example/c?api_key=SECRET_IN_COMMENT_555",
                    "elementPath": "token=SECRET_IN_PATH_111",
                    "nearbyText": "card=4242424242424242",
                    "url": "https://shop.example/checkout"
                },
                {
                    "id": "2",
                    "comment": "cart total is wrong",
                    "elementPath": "body > span.total",
                    "url": "https://shop.example/cart?api_key=SECRET_IN_URL_777"
                }
            ]
        });
        let res = app
            .oneshot(Request::post("/v1/adapters/agentation").header("content-type", "application/json").body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);

        let Ingested::Reported(issue) = rx.recv().await.unwrap() else { panic!("expected a Reported item") };
        assert!(issue.body_excerpt.contains("SECRET_IN_PATH_111"), "the channel copy keeps the raw value");
        assert!(issue.body_excerpt.contains("SECRET_IN_URL_777"), "the channel copy keeps the raw value");

        let record = std::fs::read_to_string(&path).unwrap();
        for secret in ["SECRET_IN_COMMENT_555", "SECRET_IN_PATH_111", "SECRET_IN_URL_777", "4242424242424242"] {
            assert!(!record.contains(secret), "record must never contain {secret}: {record}");
        }
        assert!(record.contains("[redacted]"), "record must carry the redaction marker: {record}");
        // Per-line masking, not whole-blob: masking the first note's URL must
        // not delete the second note's complaint from the record.
        let recorded: ReportedIssue = serde_json::from_str(record.trim()).unwrap();
        assert!(recorded.body_excerpt.contains("cart total is wrong"), "{}", recorded.body_excerpt);
        assert!(recorded.body_excerpt.contains("elementPath: body > span.total"), "{}", recorded.body_excerpt);
    }

    /// The regression this whole change had to avoid: Linear's route reads the
    /// generic shape and only that. A Linear payload whose `comment` field is
    /// a reply body must still be titled from `data.title`, not from the
    /// comment Agentation's reader would have grabbed.
    #[tokio::test]
    async fn linear_route_is_untouched_by_the_agentation_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let (state, mut rx) = IngestState::new(path.clone());
        let app = router(state);
        let body = serde_json::json!({
            "action": "create",
            "type": "Comment",
            "comment": "still happening on prod",
            "data": {"title": "crash on submit", "description": "500 when clicking submit twice", "elementPath": "not a thing on linear"}
        });
        let res = app
            .oneshot(Request::post("/v1/adapters/linear").header("content-type", "application/json").body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        let Ingested::Reported(issue) = rx.recv().await.unwrap() else { panic!("expected a Reported item") };
        assert_eq!(issue.title, "crash on submit", "the generic path titles from data.title, never from a comment field");
        assert!(
            !issue.body_excerpt.contains("elementPath"),
            "no composed Agentation context block on the generic path: {}",
            issue.body_excerpt
        );
        // `comment` outranking `data.description` here is `reported_body`'s own
        // long-standing precedence, pinned as-is: this work did not touch it.
        assert_eq!(issue.body_excerpt, "still happening on prod");
    }

    #[tokio::test]
    async fn reported_route_garbage_body_is_rejected_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let (state, _rx) = IngestState::new(path.clone());
        let app = router(state);
        let res = app
            .oneshot(Request::post("/v1/adapters/agentation").header("content-type", "application/json").body(Body::from("{nope")).unwrap())
            .await
            .unwrap();
        assert!(res.status().is_client_error());
        assert!(!path.exists() || std::fs::read_to_string(&path).unwrap().is_empty());
    }
}
