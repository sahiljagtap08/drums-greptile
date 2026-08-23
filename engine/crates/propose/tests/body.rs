//! What the PR body is allowed to say, and — more importantly — what it is
//! never allowed to leave out.

use engine_core::{
    Attribution, CapturedRequest, Claim, DeployRecord, ErrorEvent, ErrorSignature, Failure, Intake,
    Provenance, Repair, Reproduction,
};
use engine_propose::{extract_pr_url, render_body, render_title, ProposalRequest};

fn claim(text: &str, p: Provenance) -> Claim {
    Claim { text: text.to_string(), provenance: p }
}

fn req() -> ProposalRequest {
    ProposalRequest {
        failure: Failure {
            id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            service: "shop".into(),
            signature: ErrorSignature {
                error_name: "TypeError".into(),
                top_frame_file: "server.js".into(),
                top_frame_function: Some("computeTotal".into()),
            },
            first_seen_ms: 1_000,
            event_count: 12,
            sample: ErrorEvent {
                service: "shop".into(),
                occurred_at_ms: 1_000,
                error_name: "TypeError".into(),
                error_message: "Cannot read properties of undefined".into(),
                stack: "TypeError\n    at computeTotal (/srv/shop/server.js:5:20)".into(),
                request: Some(CapturedRequest {
                    method: "POST".into(),
                    path: "/api/checkout".into(),
                    content_type: Some("application/json".into()),
                    body: Some(r#"{"card":"4242424242424242"}"#.into()),
                }),
                intake: Intake::Snippet,
            },
            intake: Intake::Snippet,
            claim: claim("12 events share one signature", Provenance::Observed),
        },
        attribution: Some(Attribution {
            deploy: DeployRecord {
                sha: "abc1234567890abcdef1234567890abcdef12345".into(),
                description: "add promo codes".into(),
                author: "sahil".into(),
                deployed_at_ms: 500,
            },
            overlap_files: vec!["server.js".into()],
            minutes_after_deploy: 4,
            claim: claim("started 4 minutes after this deploy", Provenance::Inferred),
        }),
        reproduction: Some(Reproduction {
            sha: "abc1234567890abcdef1234567890abcdef12345".into(),
            reproduced: true,
            parent_clean: Some(true),
            detail: "replayed the captured request".into(),
            claims: vec![
                claim("reproduced at abc1234", Provenance::Verified),
                claim("parent commit serves the request cleanly", Provenance::Verified),
            ],
        }),
        repair: Repair {
            id: "rep_1".into(),
            failure_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            sha: "def4567890abcdef1234567890abcdef123456ab".into(),
            branch: "drums/repair-abc1234".into(),
            agent: "claude".into(),
            summary: "guard body.promo before reading .code\n\nmore detail here".into(),
            diff_stat: " server.js | 2 +-\n 1 file changed".into(),
            claims: vec![
                claim("the failing request now returns 200", Provenance::Verified),
                claim("test suite passes (14 tests)", Provenance::Verified),
            ],
        },
        base: "main".into(),
        revert_hint: Some("drums revert 01ARZ3NDEKTSV4RRFFQ69G5FAV".into()),
    }
}

#[test]
fn body_carries_every_claim_with_its_chip() {
    let body = render_body(&req());
    for (text, chip) in [
        ("12 events share one signature", "observed"),
        ("started 4 minutes after this deploy", "inferred"),
        ("reproduced at abc1234", "verified"),
        ("the failing request now returns 200", "verified"),
        ("test suite passes (14 tests)", "verified"),
    ] {
        assert!(body.contains(text), "missing claim {text:?} in:\n{body}");
        assert!(
            body.contains(&format!("`{chip}` — {text}")),
            "claim {text:?} lost its {chip} chip in:\n{body}"
        );
    }
}

#[test]
fn body_never_contains_a_request_body() {
    // The captured request carries a card number. A PR body is the single
    // most-forwarded artifact this product produces — into email, into Slack,
    // into anyone's notification history. A payload must never reach it.
    let body = render_body(&req());
    assert!(!body.contains("4242424242424242"), "PAYLOAD LEAKED:\n{body}");
    assert!(!body.contains("card"), "payload key leaked:\n{body}");
}

#[test]
fn unresolved_claims_appear_before_the_diff() {
    let mut r = req();
    r.repair
        .claims
        .push(claim("visual correctness was not checked", Provenance::Unresolved));
    let body = render_body(&r);

    let warning = body
        .find("## What is NOT verified")
        .expect("an unresolved claim must open its own section");
    let repair = body.find("## The repair").expect("repair section");
    assert!(
        warning < repair,
        "the unresolved section must come BEFORE the diff — a reviewer who stops \
         reading early must not have read only good news"
    );
    assert!(body.contains("visual correctness was not checked"));
}

#[test]
fn no_unresolved_claims_means_no_warning_section() {
    let body = render_body(&req());
    assert!(
        !body.contains("## What is NOT verified"),
        "an empty warning section invites reviewers to ignore a real one"
    );
}

#[test]
fn parent_dirty_is_stated_as_not_the_cause() {
    let mut r = req();
    r.reproduction.as_mut().unwrap().parent_clean = Some(false);
    let body = render_body(&r);
    assert!(
        body.contains("NOT the cause"),
        "a dirty parent must contradict the attribution out loud:\n{body}"
    );
}

#[test]
fn unreproduced_is_never_dressed_up() {
    let mut r = req();
    r.reproduction.as_mut().unwrap().reproduced = false;
    let body = render_body(&r);
    assert!(body.contains("NOT reproduced"), "{body}");
}

#[test]
fn title_is_boring_and_single_line() {
    let title = render_title(&req());
    assert_eq!(title, "fix(shop): guard body.promo before reading .code");
    assert!(!title.contains('\n'), "a multi-line title breaks gh pr create");
}

#[test]
fn body_says_nothing_was_merged_automatically() {
    assert!(render_body(&req()).contains("Nothing here was merged automatically"));
}

#[test]
fn url_extraction_handles_plain_output_and_json() {
    assert_eq!(
        extract_pr_url("https://github.com/o/r/pull/42\n").as_deref(),
        Some("https://github.com/o/r/pull/42")
    );
    assert_eq!(
        extract_pr_url(r#"[{"url":"https://github.com/o/r/pull/7"}]"#).as_deref(),
        Some("https://github.com/o/r/pull/7")
    );
    // A branch URL is not a PR URL, and must not be mistaken for one.
    assert_eq!(extract_pr_url("https://github.com/o/r/tree/branch"), None);
    assert_eq!(extract_pr_url(""), None);
    assert_eq!(extract_pr_url("Creating pull request for x into main"), None);
}

// --- regressions found by reading the FIRST real pull request this opened ---

#[test]
fn title_never_leaks_an_absolute_worktree_path() {
    // Verbatim shape of what codex actually wrote. It produced the title
    // `fix(pr-proof): Fixed [server.js](/private/var/folders/.../server` —
    // a leaked host path, truncated mid-path, with raw markdown in a title.
    let mut r = req();
    r.repair.summary =
        "Fixed [server.js](/private/var/folders/xc/9h_mxsv54cqcwjptqsb19pzc0000gn/T/drums-repro-01KYS36EFCQAFM6PZHCQ52D182/server.js) by guarding body.promo"
            .into();
    let title = render_title(&r);

    assert!(!title.contains("/private/var"), "leaked a host path: {title}");
    assert!(!title.contains("drums-repro-"), "leaked a worktree name: {title}");
    // `](` is the markdown-link signature. Bare parens are fine — the
    // `fix(scope):` prefix has them.
    assert!(!title.contains("]("), "raw markdown link in a title: {title}");
    assert!(!title.contains(".js)"), "a dangling link target survived: {title}");
    assert!(title.contains("server.js"), "the link TEXT should survive: {title}");
    assert!(title.chars().count() <= 72, "title too long ({}): {title}", title.chars().count());
}

#[test]
fn body_summary_is_also_scrubbed_of_host_paths() {
    let mut r = req();
    r.repair.summary = "Fixed [server.js](/private/var/folders/T/drums-repro-01K/server.js)".into();
    let body = render_body(&r);
    assert!(!body.contains("/private/var"), "the body leaked a host path:\n{body}");
    assert!(body.contains("server.js"));
}

#[test]
fn an_unchecked_parent_is_never_described_as_merely_unchecked() {
    // parent_clean: None covers BOTH "not attempted" and "attempted and
    // failed". "was not checked" implies a choice not to look, which is wrong
    // in the failure case — and the failure case is what actually happened on
    // a repo whose HEAD was the root commit.
    let mut r = req();
    r.reproduction.as_mut().unwrap().parent_clean = None;
    let body = render_body(&r);
    assert!(
        body.contains("NOT established"),
        "an unknown parent must be stated as unestablished:\n{body}"
    );
    assert!(
        body.contains("still only inferred"),
        "and must say what that costs the attribution:\n{body}"
    );
}

#[test]
fn a_summary_with_no_markdown_is_left_exactly_alone() {
    let mut r = req();
    r.repair.summary = "guard body.promo before reading .code".into();
    assert_eq!(render_title(&r), "fix(shop): guard body.promo before reading .code");
}

#[test]
fn an_absent_diff_stat_prints_no_empty_code_block() {
    // A reported-issue repair collects no diff stat, and an empty fenced block
    // renders as a grey void that reads as a rendering bug.
    let mut r = req();
    r.repair.diff_stat = String::new();
    let body = render_body(&r);
    assert!(!body.contains("```\n\n```"), "empty code block:\n{body}");
    assert!(!body.contains("```\n```"), "empty code block:\n{body}");
    assert!(body.contains("Written by `claude`"), "{body}");
}
