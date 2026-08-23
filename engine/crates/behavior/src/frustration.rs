//! Frustration out of the behavior source: rage clicks and dead clicks.
//!
//! # Why these two, and why not replays
//!
//! PostHog already computes both signals as first-class events — `$rageclick`
//! after several rapid clicks on one element, `$dead_click` when a click
//! visibly does nothing. Reading those events is arithmetic over data the
//! customer already collects. The alternative — scanning session replays with
//! a vision model — re-derives the same facts at four orders of magnitude the
//! cost, so vision stays where the doctrine put it: verifying repairs, not
//! watching video.
//!
//! # Same honesty rules as `engine_detect::observe`
//!
//! Thin evidence says nothing: the floors below refuse to observe one stray
//! dead click. Deterministic and idempotent: ids are content-derived, ids the
//! record already holds are skipped, output is sorted. And an observation is a
//! fact, not a judgment — "people rage-clicked on /pricing" carries no claim
//! about why; that is the hypothesis's job.

use std::collections::HashSet;

use engine_core::observation::{
    EvidenceKind, EvidenceRef, Kind, Observation, ScopeRef, Source, Window,
};

use crate::{BehaviorError, BehaviorSource};

pub const RAGE_EVENT: &str = "$rageclick";
pub const DEAD_EVENT: &str = "$dead_click";
const DAY_MS: u64 = 86_400_000;

/// One (event, page) group the behavior source aggregated over the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrustrationGroup {
    /// The source's event name — [`RAGE_EVENT`] or [`DEAD_EVENT`].
    pub event: String,
    /// The page path the clicks happened on. Empty when the source could not
    /// say — kept, because the count is still real.
    pub path: String,
    pub clicks: u64,
    pub sessions: u64,
    pub users: u64,
    /// One session to go look at, when the source offered one.
    pub sample_session: Option<String>,
}

/// Floors below which a group is noise rather than an observation.
///
/// Asymmetric on purpose, and calibrated against real data. The source emits
/// ONE `$rageclick` event per burst of three-plus rapid clicks, so a count of
/// one is already a whole episode of somebody hammering an element — the
/// first live read of twentyfour26 found exactly two such events for one
/// very real frustrated user, and a floor of three would have ignored them.
/// A dead click is one event per single click that visibly did nothing,
/// which also catches slow hydration and stray clicks on plain text; those
/// become a recorded fact only when they repeat across sessions.
#[derive(Debug, Clone)]
pub struct FrustrationParams {
    pub rage_min_clicks: u64,
    pub rage_min_sessions: u64,
    pub dead_min_clicks: u64,
    pub dead_min_sessions: u64,
}

impl Default for FrustrationParams {
    fn default() -> Self {
        FrustrationParams {
            rage_min_clicks: 1,
            rage_min_sessions: 1,
            dead_min_clicks: 6,
            dead_min_sessions: 2,
        }
    }
}

/// The polling window: one day, sliding with the tick. The id buckets by
/// calendar day of `now`, so a page rage-clicked all afternoon is one
/// observation, and the same page still rage-clicked tomorrow is honestly a
/// new one.
pub fn window_for(now_ms: u64) -> Window {
    Window {
        start_ms: now_ms.saturating_sub(DAY_MS),
        end_ms: now_ms,
    }
}

/// The query, built and returned as a string so a test can assert on it.
/// Explicit window rather than `now()`-relative for the same reason as
/// `completion_query_between`: a query that slides with the source's clock
/// cannot be pinned in a test or reasoned about after the fact.
pub fn frustration_query(from_ms: u64, to_ms: u64, limit: u32) -> Result<String, BehaviorError> {
    if to_ms <= from_ms {
        return Err(BehaviorError::Shape("window ends before it starts".into()));
    }
    let (from_s, to_s) = (from_ms / 1000, to_ms / 1000);
    let limit = limit.clamp(1, 1000);
    Ok(format!(
        "SELECT event, coalesce(properties.$pathname, '') AS path, \
                count() AS clicks, \
                uniq(properties.$session_id) AS sessions, \
                uniq(person_id) AS users, \
                any(toString(properties.$session_id)) AS sample_session \
         FROM events \
         WHERE timestamp >= toDateTime({from_s}) AND timestamp < toDateTime({to_s}) \
           AND event IN ('{RAGE_EVENT}', '{DEAD_EVENT}') \
         GROUP BY event, path \
         ORDER BY clicks DESC LIMIT {limit}"
    ))
}

pub fn parse_frustration(body: &serde_json::Value) -> Result<Vec<FrustrationGroup>, BehaviorError> {
    use crate::posthog::{as_u64, column_index, rows};
    let event_at = column_index(body, "event")?;
    let path_at = column_index(body, "path")?;
    let clicks_at = column_index(body, "clicks")?;
    let sessions_at = column_index(body, "sessions")?;
    let users_at = column_index(body, "users")?;
    let sample_at = column_index(body, "sample_session")?;
    Ok(rows(body)?
        .iter()
        .filter_map(serde_json::Value::as_array)
        .filter_map(|r| {
            Some(FrustrationGroup {
                event: r.get(event_at)?.as_str()?.to_string(),
                path: r
                    .get(path_at)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                clicks: as_u64(r.get(clicks_at)).unwrap_or(0),
                sessions: as_u64(r.get(sessions_at)).unwrap_or(0),
                users: as_u64(r.get(users_at)).unwrap_or(0),
                sample_session: r
                    .get(sample_at)
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty() && *s != "null")
                    .map(str::to_string),
            })
        })
        .collect())
}

/// Stable, content-derived id: same kind + page + calendar day → same id,
/// which is what makes re-running the producer idempotent against the record.
/// FNV-1a, same as `engine_detect::observe::shift_id`, for the same reason:
/// the requirement is stability, not security.
fn frustration_id(tag: &str, path: &str, day: u64) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in tag.bytes().chain([0u8]).chain(path.bytes()) {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("obs_{tag}_{day}_{h:016x}")
}

fn clamp_u32(v: u64) -> u32 {
    v.min(u32::MAX as u64) as u32
}

/// `Some(0)` would claim "counted, and it was zero" for a group that plainly
/// had events; a source that returned no count gets `None` instead — unknown
/// and zero are different claims and stay different.
fn count(v: u64) -> Option<u32> {
    (v > 0).then(|| clamp_u32(v))
}

/// Turn aggregated groups into observations, skipping what the record already
/// holds. Pure: the caller supplies the groups, the clock, and the record.
pub fn observations(
    groups: Vec<FrustrationGroup>,
    lines: &[(String, serde_json::Value)],
    now_ms: u64,
    params: &FrustrationParams,
) -> Vec<Observation> {
    // Everything already observed, so a second run emits nothing new.
    let existing: HashSet<String> = Observation::all(lines.iter())
        .into_iter()
        .map(|o| o.id.0)
        .collect();
    let day = now_ms / DAY_MS;
    let window = window_for(now_ms);

    let mut found = Vec::new();
    for g in groups {
        let (kind, tag) = match g.event.as_str() {
            RAGE_EVENT
                if g.clicks >= params.rage_min_clicks && g.sessions >= params.rage_min_sessions =>
            {
                (
                    Kind::RageClick {
                        path: g.path.clone(),
                        clicks: clamp_u32(g.clicks),
                    },
                    "rage",
                )
            }
            DEAD_EVENT
                if g.clicks >= params.dead_min_clicks && g.sessions >= params.dead_min_sessions =>
            {
                (
                    Kind::DeadClick {
                        path: g.path.clone(),
                        clicks: clamp_u32(g.clicks),
                    },
                    "dead",
                )
            }
            // Below the floor, or an event this producer does not know.
            // Either way: not an observation, and not an error.
            _ => continue,
        };
        let id = frustration_id(tag, &g.path, day);
        if existing.contains(&id) {
            continue;
        }
        let mut o = Observation::fact(id, Source::PostHog, kind, window, now_ms)
            .with_affected(count(g.users), count(g.sessions));
        if !g.path.is_empty() {
            o = o.with_scope([ScopeRef::Route(g.path.clone())]);
        }
        if let Some(s) = &g.sample_session {
            o = o.with_evidence([EvidenceRef {
                source: Source::PostHog,
                kind: EvidenceKind::Session,
                id: s.clone(),
            }]);
        }
        found.push(o);
    }
    // Sorted so map iteration order upstream can never decide the record.
    found.sort_by(|a, b| a.id.0.cmp(&b.id.0));
    found
}

/// Ask the source and turn the answer into observations. The engine calls
/// this on the observe tick; everything that can be wrong lives in the pure
/// functions above, where the tests are.
pub async fn observe_frustration(
    source_impl: &dyn BehaviorSource,
    lines: &[(String, serde_json::Value)],
    now_ms: u64,
    params: &FrustrationParams,
) -> Result<Vec<Observation>, BehaviorError> {
    let w = window_for(now_ms);
    let groups = source_impl
        .frustration_between(w.start_ms, w.end_ms, 200)
        .await?;
    Ok(observations(groups, lines, now_ms, params))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rage(path: &str, clicks: u64, sessions: u64) -> FrustrationGroup {
        FrustrationGroup {
            event: RAGE_EVENT.into(),
            path: path.into(),
            clicks,
            sessions,
            users: sessions,
            sample_session: Some(format!("s_{path}")),
        }
    }

    fn dead(path: &str, clicks: u64, sessions: u64) -> FrustrationGroup {
        FrustrationGroup {
            event: DEAD_EVENT.into(),
            ..rage(path, clicks, sessions)
        }
    }

    const NOW: u64 = 1_755_500_000_000;

    #[test]
    fn the_window_is_frozen_in_the_query_and_backwards_is_refused() {
        let q = frustration_query(1_755_400_000_000, 1_755_500_000_000, 200).unwrap();
        assert!(q.contains("toDateTime(1755400000)"), "{q}");
        assert!(q.contains("toDateTime(1755500000)"), "{q}");
        assert!(
            !q.contains("now()"),
            "a polling window must not slide inside the source: {q}"
        );
        assert!(
            q.contains("'$rageclick'") && q.contains("'$dead_click'"),
            "{q}"
        );
        assert!(frustration_query(5, 5, 200).is_err());
    }

    #[test]
    fn a_response_is_read_by_column_name_not_position() {
        // Same row, columns shuffled. Positional reading would swap counts.
        let body = json!({
            "columns": ["clicks", "path", "event", "users", "sample_session", "sessions"],
            "results": [[14, "/pricing", "$rageclick", 3, "sess_1", 3]]
        });
        let got = parse_frustration(&body).unwrap();
        assert_eq!(
            got,
            vec![FrustrationGroup {
                event: "$rageclick".into(),
                path: "/pricing".into(),
                clicks: 14,
                sessions: 3,
                users: 3,
                sample_session: Some("sess_1".into()),
            }]
        );
    }

    #[test]
    fn floors_hold_one_stray_dead_click_says_nothing() {
        let groups = vec![
            dead("/checkout", 1, 1),  // noise
            dead("/checkout2", 6, 1), // repeated, but one session
            rage("/pricing", 3, 1),   // a rage click is already deliberate
        ];
        let found = observations(groups, &[], NOW, &FrustrationParams::default());
        assert_eq!(found.len(), 1);
        assert!(
            matches!(&found[0].kind, Kind::RageClick { path, clicks: 3 } if path == "/pricing")
        );
    }

    #[test]
    fn a_repeated_dead_click_across_sessions_is_observed_with_its_context() {
        let found = observations(
            vec![dead("/settings", 9, 4)],
            &[],
            NOW,
            &FrustrationParams::default(),
        );
        assert_eq!(found.len(), 1);
        let o = &found[0];
        assert!(matches!(&o.kind, Kind::DeadClick { clicks: 9, .. }));
        assert_eq!(o.affected.sessions, Some(4));
        assert_eq!(o.scope, vec![ScopeRef::Route("/settings".into())]);
        assert_eq!(o.evidence.len(), 1);
        assert_eq!(o.evidence[0].kind, EvidenceKind::Session);
        assert_eq!(o.source, Source::PostHog);
        assert!(
            o.measure.is_none(),
            "counts live in affected, not a bare measure"
        );
    }

    #[test]
    fn an_event_this_producer_does_not_know_is_skipped_not_recorded() {
        let mut g = rage("/x", 50, 10);
        g.event = "$autocapture".into();
        assert!(observations(vec![g], &[], NOW, &FrustrationParams::default()).is_empty());
    }

    #[test]
    fn a_missing_path_still_counts_but_claims_no_scope() {
        let found = observations(
            vec![rage("", 5, 2)],
            &[],
            NOW,
            &FrustrationParams::default(),
        );
        assert_eq!(found.len(), 1);
        assert!(found[0].scope.is_empty());
    }

    #[test]
    fn the_same_day_produces_the_same_id_and_output_is_deterministic() {
        let groups = || vec![rage("/pricing", 4, 2), dead("/settings", 8, 3)];
        let a = observations(groups(), &[], NOW, &FrustrationParams::default());
        let b = observations(groups(), &[], NOW + 600_000, &FrustrationParams::default());
        assert_eq!(
            a.iter().map(|o| o.id.0.clone()).collect::<Vec<_>>(),
            b.iter().map(|o| o.id.0.clone()).collect::<Vec<_>>(),
            "a later tick the same day must not mint new ids for the same fact"
        );
        let c = observations(groups(), &[], NOW, &FrustrationParams::default());
        assert_eq!(a, c);
    }

    #[test]
    fn idempotence_holds_through_the_real_record_wrapper() {
        // Through the actual append/read path, not hand-built lines — the
        // `fact` rename in `Observation` exists because the wrapper's own
        // `kind` key once destroyed the variant on read-back and the producer
        // re-emitted forever. This test is where that cannot regress.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let first = observations(
            vec![rage("/pricing", 4, 2)],
            &[],
            NOW,
            &FrustrationParams::default(),
        );
        assert_eq!(first.len(), 1);
        for o in &first {
            engine_record::append(&path, engine_core::observation::RECORD_KIND, o, NOW).unwrap();
        }
        let read = engine_record::read_all(&path).unwrap();
        let again = observations(
            vec![rage("/pricing", 4, 2)],
            &read.lines,
            NOW + 600_000,
            &FrustrationParams::default(),
        );
        assert!(
            again.is_empty(),
            "re-emitted through the wrapper: {again:?}"
        );
    }
}
