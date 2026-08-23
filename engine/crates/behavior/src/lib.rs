//! Where workflow behavior comes from.
//!
//! # Existing analytics first
//!
//! If a team already runs PostHog and already emits the events a workflow is
//! defined on, Drums reads those. It does not ask them to install a second event
//! pipeline emitting the same facts. Drums-owned behavioral capture is the
//! fallback for teams whose events do not exist or are not specific enough —
//! never a prerequisite (see `docs/INSTALL_FLOW.md` §2a).
//!
//! That is a strategy decision rather than a convenience. "EvaluationTarget events are
//! missing" is an implementation observation; the requirement is that **Drums
//! needs behavior about this workflow**, and building a second analytics product
//! to satisfy it would be a gap list driving the roadmap. It also turns the
//! hardest competitive question into a demonstration: a team connects PostHog
//! and watches it become an input to something none of their tools do.
//!
//! # This is a pull, and that is deliberate
//!
//! The rest of Drums ingest is push — the running application posts to
//! `/v1/events`. Behavior is different in kind. A completion rate is an
//! aggregate over a window with a denominator, not a stream of arrivals, and the
//! system that already holds the events can compute it far better than we can
//! re-derive it from a copy. So [`BehaviorSource`] asks questions instead of
//! receiving pushes.
//!
//! # Everything is keyed by workflow
//!
//! There is no method here that returns "events". Every reading is a reading
//! *for a [`EvaluationId`]*, because a behavioral fact with nothing to be a fact
//! about is the shape that produced dashboards nobody acts on.

use engine_core::evaluation::{EvaluationId, EvaluationTarget, Metric, Sample};
use engine_core::observation::{EvidenceKind, EvidenceRef, Measure, Observation, Source, Window};

pub mod frustration;
pub mod plane;
pub mod posthog;

pub use plane::Plane;
pub use posthog::PostHog;

/// Read a workflow and record what was seen, as one [`Observation`].
///
/// # Why this is the only bridge
///
/// The rule is that no approximation is ever persisted, and this is where it is
/// enforced rather than remembered. If the source cannot compute the workflow's
/// metric, [`BehaviorSource::sample`] returns [`BehaviorError::UnsupportedMetric`]
/// and this returns before an `Observation` exists — there is no partially-built
/// observation to fall back on, and no "record it as unknown" path, because a
/// row saying "we did not measure this" that later gets counted or charted is
/// worse than an absent row.
///
/// `id` and `now_ms` are supplied by the caller so this stays deterministic and
/// testable; nothing here reaches for a clock or a random number.
pub async fn observe(
    source_impl: &dyn BehaviorSource,
    source: Source,
    workflow: &EvaluationTarget,
    days: u32,
    id: impl Into<String>,
    now_ms: u64,
) -> Result<Observation, BehaviorError> {
    let sample = source_impl.sample(workflow, days).await?;

    let span_ms = (days as u64).saturating_mul(86_400_000);
    let window = Window::new(now_ms.saturating_sub(span_ms), now_ms)
        .ok_or_else(|| BehaviorError::Shape("window ends before it starts".into()))?;

    let observation = Observation::reading(
        id,
        workflow.id.clone(),
        source.clone(),
        Measure {
            metric: workflow.metric,
            sample,
        },
        window,
        now_ms,
    );

    // A handful of people to go look at, not a copy of the customer's
    // behavioral data. `with_evidence` caps this regardless of what we hand it,
    // and the population size is already in `affected` and `sample`.
    //
    // The session count stays `None`. This request is capped, so its length is
    // the cap rather than a fact; and the query groups by person, so these are
    // passages, not sessions. Deriving a number from either would put a
    // confident wrong total on screen.
    let entries = source_impl
        .entries(
            workflow,
            days,
            engine_core::observation::MAX_EVIDENCE_REFS as u32,
        )
        .await
        .unwrap_or_default();
    let refs = entries
        .into_iter()
        // Only the ones who did not finish. Evidence for an observation about
        // abandonment that links to people who completed is worse than none.
        .filter(|e| e.completed_at_ms.is_none())
        .map(|e| EvidenceRef {
            source: source.clone(),
            kind: EvidenceKind::Person,
            id: e.person,
        });

    Ok(observation.with_evidence(refs))
}

/// One person's passage through a workflow, as the behavior source saw it.
///
/// `person` is whatever stable identifier the source already holds. Drums never
/// asks for and never stores a direct identifier — the install prompt tells the
/// customer's agent to send an internal ID or nothing at all — so this is opaque
/// here on purpose and must stay that way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluationEntry {
    pub evaluation: EvaluationId,
    pub person: String,
    pub started_at_ms: u64,
    /// `None` means they entered and did not finish inside the window. That is
    /// the population the whole product is interested in.
    pub completed_at_ms: Option<u64>,
}

/// An event name the source has actually seen, with how often.
///
/// This exists so the workflow creation screen can offer a picker over what
/// telemetry really contains rather than a text box that invites a typo. An
/// event name that does not exist produces a workflow whose success event never
/// fires, whose completion rate reads zero forever, and whose every screen looks
/// healthy — the `WorkflowDefinitionInvalid` blocker, avoided at the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenEvent {
    pub name: String,
    pub count: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum BehaviorError {
    #[error("could not reach the behavior source: {0}")]
    Unreachable(String),
    /// Separate from every other failure because it is the one with an obvious
    /// human fix, and telling somebody "query failed" when their key expired
    /// wastes an afternoon.
    #[error("the behavior source rejected our credentials")]
    Unauthorized,
    #[error("the behavior source rejected the query: {0}")]
    Rejected(String),
    #[error("unexpected response shape: {0}")]
    Shape(String),
    /// Refused before sending. See [`posthog::validate_event_name`].
    #[error("unsafe event name {0:?} — refusing to build a query from it")]
    UnsafeEventName(String),
    /// Named rather than approximated. A metric this source cannot compute
    /// honestly must say so; returning the closest available number is how a
    /// product starts reporting things that are not true.
    #[error("{source_name} cannot compute {metric} for a workflow yet")]
    UnsupportedMetric {
        source_name: &'static str,
        metric: &'static str,
    },
}

impl BehaviorError {
    /// Whether retrying could plausibly succeed without a human changing
    /// something. Drives whether the console shows a spinner or a blocker.
    pub fn is_transient(&self) -> bool {
        matches!(self, BehaviorError::Unreachable(_))
    }
}

/// A place that knows what users did.
///
/// PostHog is the first implementation. The trait exists so Drums-owned capture
/// can be a second one for teams who need it, and so nothing downstream ever
/// depends on which it is talking to.
#[async_trait::async_trait]
pub trait BehaviorSource: Send + Sync {
    /// For the workflow creation picker.
    async fn seen_events(&self, days: u32, limit: u32) -> Result<Vec<SeenEvent>, BehaviorError>;

    /// The target metric over a window, with the denominator it was computed
    /// from. Returning a [`Sample`] rather than an `f64` is the point: a number
    /// without its sample size cannot be refused, and refusing thin numbers is
    /// what keeps "verified" meaningful (`engine_core::evaluation::Outcome`).
    async fn sample(&self, workflow: &EvaluationTarget, days: u32)
        -> Result<Sample, BehaviorError>;

    /// The target metric over an EXPLICIT window — outcome measurement's
    /// reading. The window was declared before rollout; it does not slide with
    /// the clock, however late the measurement runs.
    async fn sample_between(
        &self,
        target: &EvaluationTarget,
        from_ms: u64,
        to_ms: u64,
    ) -> Result<Sample, BehaviorError>;

    /// Individual passages, for attaching observations to a workflow.
    async fn entries(
        &self,
        workflow: &EvaluationTarget,
        days: u32,
        limit: u32,
    ) -> Result<Vec<EvaluationEntry>, BehaviorError>;

    /// Frustration events — rage clicks and dead clicks — grouped by page over
    /// an explicit window. A refusing default rather than a required method: a
    /// source without click telemetry is a normal state, the caller treats
    /// exactly this error as "skip quietly", and a second implementation only
    /// overrides this once it can answer honestly.
    async fn frustration_between(
        &self,
        from_ms: u64,
        to_ms: u64,
        limit: u32,
    ) -> Result<Vec<frustration::FrustrationGroup>, BehaviorError> {
        let _ = (from_ms, to_ms, limit);
        Err(BehaviorError::UnsupportedMetric {
            source_name: "this behavior source",
            metric: "frustration events",
        })
    }
}

/// Turn an entered/completed count into the metric a workflow declared.
///
/// Shared rather than duplicated per source so two sources cannot disagree about
/// what "completion rate" means — the same reason the console renders `doctor`
/// instead of growing its own health model.
///
/// `entered` is always the denominator, including for [`Metric::Abandonment`],
/// so a workflow that swaps its metric keeps a comparable sample size.
pub fn rate_for(metric: Metric, entered: u64, completed: u64) -> Result<Sample, BehaviorError> {
    // Completions are a subset of entries by construction — the query only
    // counts a completion for somebody it already counted as entering. If that
    // ever stops being true it is a bug in the query, and the two ways to
    // absorb it here are both worse than refusing: clamping hides the bug, and
    // dividing produces a completion rate above 100%, which is the single most
    // obviously wrong number this product could put on a screen.
    if completed > entered {
        return Err(BehaviorError::Shape(format!(
            "{completed} completions against {entered} entries — completions must be a subset"
        )));
    }
    let value = match metric {
        // Guarding the divide rather than letting it produce NaN: a NaN would
        // propagate into an outcome comparison and compare false against
        // everything, silently turning "no traffic" into "not an improvement".
        Metric::CompletionRate if entered == 0 => 0.0,
        Metric::CompletionRate => completed as f64 / entered as f64,
        Metric::Abandonment if entered == 0 => 0.0,
        Metric::Abandonment => (entered.saturating_sub(completed)) as f64 / entered as f64,
        other => {
            return Err(BehaviorError::UnsupportedMetric {
                source_name: "this behavior source",
                metric: other.label(),
            })
        }
    };
    Ok(Sample {
        value,
        entries: entered.min(u32::MAX as u64) as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_and_abandonment_are_complements_over_the_same_denominator() {
        let done = rate_for(Metric::CompletionRate, 400, 312).unwrap();
        let gone = rate_for(Metric::Abandonment, 400, 312).unwrap();
        assert!((done.value + gone.value - 1.0).abs() < 1e-9);
        assert_eq!(done.entries, 400);
        assert_eq!(
            gone.entries, 400,
            "abandonment keeps entries as the denominator"
        );
    }

    #[test]
    fn no_traffic_is_zero_and_not_nan() {
        // A NaN here would compare false against every threshold downstream and
        // quietly read as "did not improve" rather than "nothing happened".
        let s = rate_for(Metric::CompletionRate, 0, 0).unwrap();
        assert_eq!(s.value, 0.0);
        assert!(!s.value.is_nan());
        assert_eq!(s.entries, 0);
    }

    #[test]
    fn a_metric_we_cannot_compute_is_an_error_not_an_approximation() {
        let err = rate_for(Metric::TimeToComplete, 100, 50).unwrap_err();
        assert!(matches!(err, BehaviorError::UnsupportedMetric { .. }));
        assert!(!err.is_transient());
    }

    #[test]
    fn completions_exceeding_entries_is_refused_rather_than_rendered() {
        // Would arise if a query ever counted a completion whose start fell
        // outside the window. Clamping would hide the bug; dividing would put
        // "completion rate 130%" on a customer's screen. Neither is acceptable,
        // so it is loud.
        for metric in [Metric::CompletionRate, Metric::Abandonment] {
            let err = rate_for(metric, 100, 130).unwrap_err();
            assert!(
                matches!(err, BehaviorError::Shape(_)),
                "{metric:?} did not refuse"
            );
        }
    }

    #[test]
    fn everyone_completing_is_exactly_one_and_not_an_error() {
        // The boundary the check above must not eat.
        let s = rate_for(Metric::CompletionRate, 250, 250).unwrap();
        assert_eq!(s.value, 1.0);
        assert_eq!(rate_for(Metric::Abandonment, 250, 250).unwrap().value, 0.0);
    }
}
