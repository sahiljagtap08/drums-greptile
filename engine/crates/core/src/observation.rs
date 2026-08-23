//! What Drums saw. The root of `Observation → Hypothesis → Change → Outcome`.
//!
//! # An observation is not a problem
//!
//! An observation is a **fact about a window**:
//!
//! > completion rate is 61% over 340 entrants
//! > 31 admins repeatedly bounced between permissions and invites
//! > 9 users repeated the same step
//! > error rate rose after deploy `a1b2c3`
//!
//! None of those is a bug, and none of them says why. The reasoning happens
//! afterwards, in a hypothesis that *cites* an observation. Collapsing the two —
//! recording "the upload step is broken" instead of "24% abandon between upload
//! and quote" — means Drums has decided what is wrong before it has thought
//! about it, and every downstream artifact inherits that guess as if it were
//! evidence. So there is no `severity` here, no `problem`, no title written in
//! the voice of a diagnosis.
//!
//! # An observation is not scoped to a pre-declared anything
//!
//! The company is: Drums understands how people use the software, discovers
//! what can be improved, changes it, and measures whether the change helped.
//! If an observation required a customer-defined workflow before it could
//! exist, Drums could never discover anything the customer had not already
//! configured — every signal would be forced into a structure a human built
//! first, and the product would quietly become funnel-optimization software.
//!
//! So an observation belongs to the **product** (the record it is appended to —
//! one record per installed product, the same implicit anchoring every other
//! record kind already uses), and carries [`Scope`] as something Drums
//! *attaches*: an evaluation target it was read against, a journey it was
//! inferred to belong to, a route, a cohort, a release. Scope can be empty.
//! "Seen, not yet situated" is a legitimate state for evidence, and it is the
//! state most discovery starts in.
//!
//! # Properties held in the type system
//!
//! - A window, always, and [`Window::new`] refuses an end before its start.
//! - Every *rate* carries its denominator: a quantitative reading is a
//!   [`Measure`], which contains a [`Sample`], and there is no way to record a
//!   bare `f64`. Counts ([`Affected`]) are complete facts without one — "31
//!   users did X" is interpretable alone; "24% abandoned" is not.
//! - An explicit [`Source`].
//! - **Never `verified`.** [`Observation::provenance`] returns
//!   [`Provenance::Observed`] and has no other arm. Verification is what an
//!   `Outcome` earns after a change ships and its window closes.
//! - Evidence is **references**, never copies. See [`EvidenceRef`].
//! - Scope and status changes are **additive record lines**, never mutations —
//!   the record is append-only, so later knowledge about an observation is
//!   written beside it, not into it.
//!
//! # No approximations get persisted
//!
//! A [`Measure`] for a metric the behavior source cannot compute does not exist
//! to construct — `rate_for` returns `UnsupportedMetric` instead. "We could not
//! measure this, so we wrote down roughly what we think" has no representation
//! here.

use serde::{Deserialize, Serialize};

use crate::evaluation::{EvaluationId, Metric, Sample};
use crate::Provenance;

/// The record `kind` an [`Observation`] is appended under.
pub const RECORD_KIND: &str = "observation";
/// The record `kind` a [`ScopeAttached`] line is appended under.
pub const SCOPE_KIND: &str = "observation_scope";
/// The record `kind` a [`StatusChanged`] line is appended under.
pub const STATUS_KIND: &str = "observation_status";

/// How many evidence references an observation may carry.
pub const MAX_EVIDENCE_REFS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ObservationId(pub String);

impl std::fmt::Display for ObservationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which system saw it. Explicit rather than inferred, because "where did this
/// number come from" is the first question anyone asks about a number they do
/// not like.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The customer's existing product analytics.
    PostHog,
    /// The application itself, through Drums request capture.
    Runtime,
    /// A support tool.
    Support,
    /// Drums-owned behavioral capture — the fallback for teams whose analytics
    /// cannot answer (`docs/INSTALL_FLOW.md` §2a), not the default.
    DrumsCapture,
    Other(String),
}

impl Source {
    pub fn label(&self) -> &str {
        match self {
            Source::PostHog => "posthog",
            Source::Runtime => "runtime",
            Source::Support => "support",
            Source::DrumsCapture => "drums capture",
            Source::Other(s) => s,
        }
    }
}

/// The period an observation describes. Half-open: `[start, end)`.
///
/// Mandatory because a metric without a window is not interpretable — "61%
/// completion" means nothing until you know whether that is today or since
/// launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub start_ms: u64,
    pub end_ms: u64,
}

impl Window {
    /// `None` when `end` precedes `start`. A backwards window would silently
    /// produce a zero-length period that reads as "no traffic".
    pub fn new(start_ms: u64, end_ms: u64) -> Option<Window> {
        (end_ms >= start_ms).then_some(Window { start_ms, end_ms })
    }

    pub fn duration_ms(&self) -> u64 {
        self.end_ms - self.start_ms
    }
}

/// Something an observation is *about*, attached by Drums — at creation when
/// the reading was taken against a known target, or later, as a
/// [`ScopeAttached`] line, once Drums has situated a discovered fact.
///
/// Deliberately an open, growable set: journeys and the rest of the product
/// model are things Drums infers, and none of them is a prerequisite for
/// observing. The one declared variant is [`ScopeRef::Evaluation`] — a target a
/// human or a hypothesis explicitly created to measure against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", content = "id", rename_all = "snake_case")]
pub enum ScopeRef {
    /// A declared evaluation target (`crate::evaluation::EvaluationTarget`).
    Evaluation(EvaluationId),
    /// An inferred journey in the product model.
    Journey(String),
    /// A route or page.
    Route(String),
    /// A named feature or product surface.
    Feature(String),
    /// A cohort of users.
    Cohort(String),
    /// A release, by sha.
    Release(String),
    /// A code surface, by path.
    Code(String),
}

/// A later-attached scope, as its own append-only record line.
///
/// The record never mutates, so knowledge acquired after an observation was
/// written — "this belongs to the team-onboarding journey" — is appended beside
/// it and folded in at read time by [`Observation::scoped`]. `provenance` says
/// how the attachment was earned: `Inferred` when Drums concluded it,
/// `Observed` when the data states it outright. An attachment can never claim
/// `Verified` any more than the observation itself can.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeAttached {
    pub observation: ObservationId,
    #[serde(flatten)]
    pub scope: ScopeRef,
    pub provenance: Provenance,
    /// One sentence of why — "permissions and invites are both steps of the
    /// invite journey", not a wall of reasoning.
    pub basis: String,
}

/// A quantitative claim: which metric, its value, and the denominator it was
/// computed over. One unit on purpose — `Option<Metric>` beside
/// `Option<Sample>` would permit a metric with no sample, which is exactly the
/// bare-number shape this module refuses to represent.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Measure {
    pub metric: Metric,
    pub sample: Sample,
}

/// What sort of fact this is. A classifier, not a judgment — every variant
/// describes something that happened, and none of them says it is bad.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kind {
    /// A metric, read over the window against an evaluation target.
    MetricReading,
    /// Entrants who stopped after a named point without reaching success.
    DropOff { after: String },
    /// Users who repeated a step before moving on.
    RepeatedStep { step: String },
    /// Users moving back and forth between two surfaces without progressing.
    BounceLoop { between: (String, String) },
    /// The metric moved relative to an earlier window. `previous` is that
    /// window's value; `since_deploy` is a correlation, deliberately not called
    /// a cause — attributing it is a later step with its own evidence.
    RateShift {
        previous: f64,
        since_deploy: Option<String>,
    },
    /// The same element clicked over and over in rapid succession, as the
    /// behavior source already detects it. `clicks` is the event count over
    /// the window; the people and sessions behind it live in `affected`.
    RageClick { path: String, clicks: u32 },
    /// Clicks that visibly did nothing — no navigation, no DOM response.
    /// Same counting rules as [`Kind::RageClick`].
    DeadClick { path: String, clicks: u32 },
}

/// How many were touched. Counts, not rates — complete facts on their own.
///
/// Both fields are `Option` because not every source can count them, and the
/// alternative is worse than a missing field: a source that groups by person
/// has no session count, an error-event stream carries no user identity at
/// all, and writing `0` in either place renders as "nobody was affected" —
/// a confident wrong number, which is the exact failure this product exists
/// to avoid. Unknown and zero are different claims and stay different.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Affected {
    #[serde(default)]
    pub users: Option<u32>,
    #[serde(default)]
    pub sessions: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Person,
    Session,
    Recording,
    Event,
    Deploy,
}

/// A pointer into the system that already holds the evidence.
///
/// **This carries an identifier and nothing else, on purpose.** PostHog already
/// owns session history, recordings and event properties; copying that into
/// Drums would turn an improvement loop into a second-rate user-tracking
/// database, with all of the retention and privacy obligations and none of the
/// features. There is deliberately no URL field either: the link is derivable
/// from the connection's host and project, and storing derived data is how two
/// copies of the truth start disagreeing after a workspace migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub source: Source,
    pub kind: EvidenceKind,
    /// Opaque, owned by `source`. Never parsed here.
    pub id: String,
}

/// Where an observation is in the loop. Note what is absent: there is no
/// variant that claims anything got better.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Status {
    /// Recorded. Nobody has reasoned about it yet.
    Open,
    /// A hypothesis cites this observation. The reasoning lives there.
    Hypothesized { hypothesis: String },
    /// A human decided not to pursue it. Kept, not deleted — an observation
    /// dismissed three times is itself a finding.
    Dismissed { reason: String },
}

/// A status transition, as its own append-only record line. Same mechanism as
/// [`ScopeAttached`], for the same reason: the row never mutates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusChanged {
    pub observation: ObservationId,
    #[serde(flatten)]
    pub status: Status,
}

/// A fact Drums saw about the product over one window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub id: ObservationId,
    pub observed_at_ms: u64,
    pub window: Window,
    pub source: Source,
    /// Serialized as `fact`, not `kind`, and the rename is load-bearing: the
    /// record wrapper (`engine_record::append`) flattens this struct beside its
    /// own top-level `"kind":"observation"` discriminator, and two `kind` keys
    /// in one flattened object silently destroy each other — the observation's
    /// variant was overwritten by the wrapper's string, read-back failed, and
    /// the producer re-emitted the same observation every tick because it
    /// could not see the one it had already written.
    #[serde(rename = "fact")]
    pub kind: Kind,
    /// Present when the fact is a rate; absent when it is a pattern whose
    /// quantities are counts in `affected`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measure: Option<Measure>,
    pub affected: Affected,
    /// At most [`MAX_EVIDENCE_REFS`]; the counts live in `affected`.
    pub evidence: Vec<EvidenceRef>,
    /// What this is about, as far as is known at write time. Empty is legal:
    /// discovery starts unsituated. More scope arrives as [`ScopeAttached`]
    /// lines, folded in by [`Observation::scoped`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scope: Vec<ScopeRef>,
    pub status: Status,
}

impl Observation {
    /// A metric reading against a declared evaluation target — what a behavior
    /// source produces. Requires a full [`Measure`]; there is no reading
    /// without one, which is how "no approximation is persisted" is enforced
    /// rather than remembered.
    pub fn reading(
        id: impl Into<String>,
        target: EvaluationId,
        source: Source,
        measure: Measure,
        window: Window,
        observed_at_ms: u64,
    ) -> Observation {
        let affected = Affected {
            users: Some(affected_from(measure)),
            sessions: None,
        };
        Observation {
            id: ObservationId(id.into()),
            observed_at_ms,
            window,
            source,
            kind: Kind::MetricReading,
            measure: Some(measure),
            affected,
            evidence: Vec::new(),
            scope: vec![ScopeRef::Evaluation(target)],
            status: Status::Open,
        }
    }

    /// A discovered pattern — no declared target, no metric, possibly no scope
    /// at all yet. This constructor existing is the correction: Drums may
    /// observe before anyone has configured anything.
    pub fn fact(
        id: impl Into<String>,
        source: Source,
        kind: Kind,
        window: Window,
        observed_at_ms: u64,
    ) -> Observation {
        Observation {
            id: ObservationId(id.into()),
            observed_at_ms,
            window,
            source,
            kind,
            measure: None,
            affected: Affected {
                users: None,
                sessions: None,
            },
            evidence: Vec::new(),
            scope: Vec::new(),
            status: Status::Open,
        }
    }

    pub fn with_scope(mut self, scope: impl IntoIterator<Item = ScopeRef>) -> Self {
        self.scope = scope.into_iter().collect();
        self
    }

    /// Attach the quantitative reading a fact-shaped constructor did not carry.
    /// Takes a whole [`Measure`] — metric plus sample — so a value can never be
    /// attached without its denominator.
    pub fn with_measure(mut self, measure: Measure) -> Self {
        self.measure = Some(measure);
        self
    }

    /// `None` means the source cannot count that population. `Some(0)` means
    /// genuinely zero; the two are different claims and stay different.
    pub fn with_affected(mut self, users: Option<u32>, sessions: Option<u32>) -> Self {
        self.affected = Affected { users, sessions };
        self
    }

    /// Attach references, truncated to [`MAX_EVIDENCE_REFS`]. Truncation rather
    /// than rejection: a caller with 4,000 affected sessions is not doing
    /// anything wrong — it just must not put all of them here. The population
    /// size is already recorded in `affected`; these are the handful a human
    /// opens to see the thing for themselves.
    pub fn with_evidence(mut self, refs: impl IntoIterator<Item = EvidenceRef>) -> Self {
        self.evidence = refs.into_iter().take(MAX_EVIDENCE_REFS).collect();
        self
    }

    /// Always [`Provenance::Observed`]. There is no other arm and there must
    /// never be one: `verified` is what an outcome earns by measuring a change
    /// after it ships, and an observation claiming it would let a reading of
    /// how things are right now pass as proof that something got better.
    pub fn provenance(&self) -> Provenance {
        Provenance::Observed
    }

    /// All observations in a decoded record, newest knowledge included —
    /// which today means the rows as written; scope attachments are folded by
    /// [`Observation::scoped`], status by [`Observation::current_status`].
    pub fn all<'a>(
        lines: impl IntoIterator<Item = &'a (String, serde_json::Value)>,
    ) -> Vec<Observation> {
        lines
            .into_iter()
            .filter(|(kind, _)| kind == RECORD_KIND)
            .filter_map(|(_, v)| serde_json::from_value::<Observation>(v.clone()).ok())
            .collect()
    }

    /// Observations about `scope` — whether the row said so at write time or a
    /// [`ScopeAttached`] line said so later. One fold, so nothing drawing a
    /// scoped view can disagree with the record about what belongs to it.
    pub fn scoped<'a>(
        lines: impl IntoIterator<Item = &'a (String, serde_json::Value)> + Clone,
        scope: &ScopeRef,
    ) -> Vec<Observation> {
        let attached: std::collections::HashSet<ObservationId> = lines
            .clone()
            .into_iter()
            .filter(|(kind, _)| kind == SCOPE_KIND)
            .filter_map(|(_, v)| serde_json::from_value::<ScopeAttached>(v.clone()).ok())
            .filter(|a| &a.scope == scope)
            .map(|a| a.observation)
            .collect();
        Self::all(lines)
            .into_iter()
            .filter(|o| o.scope.contains(scope) || attached.contains(&o.id))
            .collect()
    }

    /// The latest status for an observation: the row's own, unless
    /// [`StatusChanged`] lines were appended after it — last one wins.
    pub fn current_status<'a>(
        lines: impl IntoIterator<Item = &'a (String, serde_json::Value)>,
        id: &ObservationId,
    ) -> Option<Status> {
        let mut status = None;
        for (kind, v) in lines {
            if kind == RECORD_KIND {
                if let Ok(o) = serde_json::from_value::<Observation>(v.clone()) {
                    if &o.id == id && status.is_none() {
                        status = Some(o.status);
                    }
                }
            } else if kind == STATUS_KIND {
                if let Ok(s) = serde_json::from_value::<StatusChanged>(v.clone()) {
                    if &s.observation == id {
                        status = Some(s.status);
                    }
                }
            }
        }
        status
    }
}

/// Entrants a reading is *about*: for a higher-is-better metric, the ones who
/// did not get there; for a lower-is-better one, the ones it counted. Clamped
/// because a value outside [0,1] would otherwise produce an affected count
/// above the denominator or below zero, and both render as nonsense.
fn affected_from(measure: Measure) -> u32 {
    let share = match measure.metric.better() {
        crate::evaluation::Better::Higher => 1.0 - measure.sample.value,
        crate::evaluation::Better::Lower => measure.sample.value,
    };
    (share.clamp(0.0, 1.0) * measure.sample.entries as f64).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval() -> EvaluationId {
        EvaluationId("eval_checkout".into())
    }

    fn reading() -> Observation {
        Observation::reading(
            "obs_184",
            eval(),
            Source::PostHog,
            Measure {
                metric: Metric::CompletionRate,
                sample: Sample {
                    value: 0.61,
                    entries: 340,
                },
            },
            Window::new(1_754_000_000_000, 1_754_172_800_000).unwrap(),
            1_754_172_800_000,
        )
    }

    #[test]
    fn an_observation_can_never_be_verified() {
        // The load-bearing test of this module. `verified` is what an Outcome
        // earns by measuring a change after it ships. If an observation could
        // claim it, a reading of the present would pass as proof of improvement.
        let mut o = reading();
        for status in [
            Status::Open,
            Status::Hypothesized {
                hypothesis: "h_1".into(),
            },
            Status::Dismissed {
                reason: "seasonal".into(),
            },
        ] {
            o.status = status;
            assert_eq!(o.provenance(), Provenance::Observed);
            let json = serde_json::to_string(&o).unwrap();
            assert!(!json.contains("verified"), "a status serialized to: {json}");
        }
    }

    #[test]
    fn an_observation_needs_no_scope_to_exist() {
        // The correction this module was rebuilt around. Drums may observe
        // something interesting before anyone — customer or Drums — has decided
        // what it belongs to. Requiring a pre-declared workflow here would make
        // discovery unrepresentable.
        let o = Observation::fact(
            "obs_1",
            Source::PostHog,
            Kind::BounceLoop {
                between: ("permissions".into(), "invites".into()),
            },
            Window::new(0, 86_400_000).unwrap(),
            86_400_000,
        )
        .with_affected(Some(31), None);
        assert!(o.scope.is_empty());
        assert!(o.measure.is_none());
        let json = serde_json::to_string(&o).unwrap();
        let back: Observation = serde_json::from_str(&json).unwrap();
        assert_eq!(o, back);
        // And an empty scope is omitted from the wire, not written as [].
        assert!(!json.contains("\"scope\""));
    }

    #[test]
    fn a_reading_carries_its_evaluation_as_scope() {
        let o = reading();
        assert_eq!(o.scope, vec![ScopeRef::Evaluation(eval())]);
        assert!(o.measure.is_some());
    }

    #[test]
    fn a_rate_cannot_be_recorded_without_its_denominator() {
        // Measure couples metric and sample into one unit, so `Some(metric)`
        // with no sample is unrepresentable.
        let o = reading();
        let json: serde_json::Value = serde_json::to_value(&o).unwrap();
        assert!(json["measure"]["sample"]["entries"].is_number());
        assert!(json["measure"]["sample"]["value"].is_number());
    }

    #[test]
    fn a_backwards_window_is_refused() {
        assert!(Window::new(1_000, 999).is_none());
        assert_eq!(Window::new(1_000, 1_000).unwrap().duration_ms(), 0);
    }

    #[test]
    fn affected_counts_the_entrants_the_reading_is_about() {
        // 61% completion over 340 → 133 did not complete.
        assert_eq!(reading().affected.users, Some(133));
        // For a lower-is-better metric the value IS the affected share.
        let o = Observation::reading(
            "o",
            eval(),
            Source::PostHog,
            Measure {
                metric: Metric::Abandonment,
                sample: Sample {
                    value: 0.24,
                    entries: 500,
                },
            },
            Window::new(0, 1).unwrap(),
            1,
        );
        assert_eq!(o.affected.users, Some(120));
    }

    #[test]
    fn an_uncounted_session_total_is_none_and_not_zero() {
        let o = reading();
        assert_eq!(o.affected.sessions, None);
        assert_ne!(o.affected.sessions, Some(0), "unknown is not zero");
    }

    #[test]
    fn evidence_is_capped_so_this_does_not_become_a_tracking_database() {
        let many: Vec<EvidenceRef> = (0..500)
            .map(|i| EvidenceRef {
                source: Source::PostHog,
                kind: EvidenceKind::Session,
                id: format!("s_{i}"),
            })
            .collect();
        let o = reading()
            .with_affected(Some(133), Some(210))
            .with_evidence(many);
        assert_eq!(o.evidence.len(), MAX_EVIDENCE_REFS);
        assert_eq!(o.affected.sessions, Some(210));
    }

    #[test]
    fn an_evidence_ref_carries_no_behavioral_payload() {
        // If this struct ever grows a field holding event properties, session
        // contents, or user attributes, this test should be the thing that
        // makes someone argue for it out loud.
        let json = serde_json::to_value(EvidenceRef {
            source: Source::PostHog,
            kind: EvidenceKind::Recording,
            id: "rec_1".into(),
        })
        .unwrap();
        let mut keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["id", "kind", "source"]);
    }

    #[test]
    fn kind_classifies_without_diagnosing() {
        let o = Observation::fact(
            "o",
            Source::PostHog,
            Kind::RateShift {
                previous: 0.74,
                since_deploy: Some("a1b2c3".into()),
            },
            Window::new(0, 1).unwrap(),
            1,
        );
        let json = serde_json::to_string(&o).unwrap();
        assert!(json.contains("rate_shift"));
        assert!(!json.contains("cause"));
        assert!(!json.contains("severity"));
    }

    #[test]
    fn scope_attached_later_is_found_by_the_scope_filter() {
        // The append-only path: the observation was written unsituated, and
        // Drums later inferred where it belongs. The row never changes; a
        // ScopeAttached line lands beside it; the scoped view folds both.
        let o = Observation::fact(
            "obs_9",
            Source::PostHog,
            Kind::BounceLoop {
                between: ("permissions".into(), "invites".into()),
            },
            Window::new(0, 1).unwrap(),
            1,
        );
        let journey = ScopeRef::Journey("team-onboarding".into());
        let attach = ScopeAttached {
            observation: o.id.clone(),
            scope: journey.clone(),
            provenance: Provenance::Inferred,
            basis: "permissions and invites are both steps of the invite journey".into(),
        };
        let lines = [
            (RECORD_KIND.to_string(), serde_json::to_value(&o).unwrap()),
            (
                SCOPE_KIND.to_string(),
                serde_json::to_value(&attach).unwrap(),
            ),
        ];
        let got = Observation::scoped(lines.iter(), &journey);
        assert_eq!(got, vec![o.clone()]);
        // And a scope nothing references returns nothing.
        let other = ScopeRef::Journey("checkout".into());
        assert!(Observation::scoped(lines.iter(), &other).is_empty());
    }

    #[test]
    fn write_time_scope_and_attached_scope_are_the_same_to_a_reader() {
        let a = reading(); // scope on the row
        let b = Observation::fact(
            "obs_2",
            Source::PostHog,
            Kind::MetricReading,
            Window::new(0, 1).unwrap(),
            1,
        );
        let attach = ScopeAttached {
            observation: b.id.clone(),
            scope: ScopeRef::Evaluation(eval()),
            provenance: Provenance::Inferred,
            basis: "read against the checkout target".into(),
        };
        let lines = [
            (RECORD_KIND.to_string(), serde_json::to_value(&a).unwrap()),
            (RECORD_KIND.to_string(), serde_json::to_value(&b).unwrap()),
            (
                SCOPE_KIND.to_string(),
                serde_json::to_value(&attach).unwrap(),
            ),
        ];
        let got = Observation::scoped(lines.iter(), &ScopeRef::Evaluation(eval()));
        assert_eq!(
            got.len(),
            2,
            "row-scope and line-scope must be indistinguishable to readers"
        );
    }

    #[test]
    fn status_changes_are_lines_and_the_last_one_wins() {
        let o = reading();
        let hypothesized = StatusChanged {
            observation: o.id.clone(),
            status: Status::Hypothesized {
                hypothesis: "h_1".into(),
            },
        };
        let dismissed = StatusChanged {
            observation: o.id.clone(),
            status: Status::Dismissed {
                reason: "seasonal".into(),
            },
        };
        let lines = [
            (RECORD_KIND.to_string(), serde_json::to_value(&o).unwrap()),
            (
                STATUS_KIND.to_string(),
                serde_json::to_value(&hypothesized).unwrap(),
            ),
            (
                STATUS_KIND.to_string(),
                serde_json::to_value(&dismissed).unwrap(),
            ),
        ];
        assert_eq!(
            Observation::current_status(lines.iter(), &o.id),
            Some(Status::Dismissed {
                reason: "seasonal".into()
            })
        );
        // With no transition lines, the row's own status stands.
        let only_row = [(RECORD_KIND.to_string(), serde_json::to_value(&o).unwrap())];
        assert_eq!(
            Observation::current_status(only_row.iter(), &o.id),
            Some(Status::Open)
        );
    }

    #[test]
    fn an_observation_round_trips_through_the_record() {
        let o = reading().with_evidence([EvidenceRef {
            source: Source::PostHog,
            kind: EvidenceKind::Session,
            id: "s_1".into(),
        }]);
        let lines = [
            (RECORD_KIND.to_string(), serde_json::to_value(&o).unwrap()),
            ("deploy".to_string(), serde_json::json!({"sha": "abc"})),
        ];
        let got = Observation::scoped(lines.iter(), &ScopeRef::Evaluation(eval()));
        assert_eq!(got, vec![o]);
    }
}
