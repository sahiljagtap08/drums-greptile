//! Record lines → typed graph. Pure and deterministic: same lines, same
//! graph, every time — that determinism is what makes the index rebuildable
//! and therefore allowed to exist beside an append-only record.

use engine_core::bet::{BetStatus, BetStatusChanged, ProductBet};
use engine_core::change::{Change, OutcomeRecorded, OUTCOME_KIND};
use engine_core::hypothesis::Hypothesis;
use engine_core::observation::{EvidenceKind, Observation};
use engine_core::DeployRecord;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Service,
    ErrorSignature,
    Deploy,
    Person,
    Metric,
    Observation,
    Hypothesis,
    Bet,
    Change,
    Outcome,
    Learning,
}

impl NodeKind {
    pub fn label(&self) -> &'static str {
        match self {
            NodeKind::Service => "service",
            NodeKind::ErrorSignature => "error",
            NodeKind::Deploy => "deploy",
            NodeKind::Person => "person",
            NodeKind::Metric => "metric",
            NodeKind::Observation => "observation",
            NodeKind::Hypothesis => "hypothesis",
            NodeKind::Bet => "bet",
            NodeKind::Change => "change",
            NodeKind::Outcome => "outcome",
            NodeKind::Learning => "learning",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// `"{kind}:{natural id}"` — stable across rebuilds because every part
    /// comes from the record, none from a clock or a counter.
    pub id: String,
    pub kind: NodeKind,
    /// One line a person can read in a search result.
    pub label: String,
    /// The product scope — one record file is one product.
    pub product: String,
    /// The opaque person this node is about, when it is about one. Drums
    /// never holds a direct identifier; this is whatever opaque id the
    /// customer's own telemetry already used.
    pub person: Option<String>,
    pub at_ms: Option<u64>,
    /// The searchable text. Assembled from record fields verbatim — never
    /// summarized, never paraphrased, so a hit is always a quote.
    pub body: String,
}

/// A typed, directed edge. The relation vocabulary is closed on purpose:
/// every relation here is one the record actually asserts, and a growing
/// open set of relations is how a graph stops meaning anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub src: String,
    pub rel: &'static str,
    pub dst: String,
}

#[derive(Debug, Default)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

impl Graph {
    fn node(&mut self, node: Node) {
        // First writer wins: nodes are keyed by natural id, and the earliest
        // record line is the one that defined the thing.
        if !self.nodes.iter().any(|n| n.id == node.id) {
            self.nodes.push(node);
        }
    }
    fn edge(&mut self, src: &str, rel: &'static str, dst: &str) {
        let e = Edge { src: src.into(), rel, dst: dst.into() };
        if !self.edges.contains(&e) {
            self.edges.push(e);
        }
    }
}

/// The subset of an ingested error event the graph cares about. Mirrors
/// `engine_core::ErrorEvent`'s serialized shape without borrowing the whole
/// type — extraction reads record JSON, and a partial decode that tolerates
/// missing optional fields is the point.
#[derive(Deserialize)]
struct EventLine {
    service: String,
    error_name: String,
    error_message: String,
    #[serde(default)]
    occurred_at_ms: Option<u64>,
}

/// Build the graph from decoded record lines. `product` scopes every node —
/// one record is one product, and the column exists so the hosted plane can
/// merge stores without guessing.
pub fn extract(lines: &[(String, serde_json::Value)], product: &str) -> Graph {
    let mut g = Graph::default();

    for (kind, value) in lines {
        match kind.as_str() {
            "event" => {
                let Ok(ev) = serde_json::from_value::<EventLine>(value.clone()) else { continue };
                let service_id = format!("service:{}", ev.service);
                g.node(Node {
                    id: service_id.clone(),
                    kind: NodeKind::Service,
                    label: ev.service.clone(),
                    product: product.into(),
                    person: None,
                    at_ms: None,
                    body: format!("service {}", ev.service),
                });
                let sig = format!("error:{}:{}", ev.service, ev.error_name);
                g.node(Node {
                    id: sig.clone(),
                    kind: NodeKind::ErrorSignature,
                    label: format!("{} in {}", ev.error_name, ev.service),
                    product: product.into(),
                    person: None,
                    at_ms: ev.occurred_at_ms,
                    body: format!("{}: {}", ev.error_name, ev.error_message),
                });
                g.edge(&sig, "occurred_in", &service_id);
            }
            "deploy" => {
                let Ok(d) = serde_json::from_value::<DeployRecord>(value.clone()) else { continue };
                g.node(Node {
                    id: format!("deploy:{}", d.sha),
                    kind: NodeKind::Deploy,
                    label: format!("deploy {}", &d.sha[..d.sha.len().min(10)]),
                    product: product.into(),
                    person: None,
                    at_ms: Some(d.deployed_at_ms),
                    body: format!("deploy {} \"{}\" by {}", d.sha, d.description, d.author),
                });
            }
            _ => {}
        }
    }

    for o in Observation::all(lines.iter()) {
        let id = format!("observation:{}", o.id.0);
        let fact = serde_json::to_value(&o.kind)
            .ok()
            .map(|v| v.to_string())
            .unwrap_or_default();
        let measure = o
            .measure
            .map(|m| format!(" · {} {} over {}", m.metric.label(), m.sample.value, m.sample.entries))
            .unwrap_or_default();
        g.node(Node {
            id: id.clone(),
            kind: NodeKind::Observation,
            label: format!("observation {}", o.id.0),
            product: product.into(),
            person: None,
            at_ms: Some(o.observed_at_ms),
            body: format!("{fact}{measure}"),
        });
        if let engine_core::observation::Kind::RateShift { since_deploy: Some(sha), .. } = &o.kind {
            g.edge(&id, "correlates_with", &format!("deploy:{sha}"));
        }
        for evidence in &o.evidence {
            if evidence.kind == EvidenceKind::Person {
                let pid = format!("person:{}", evidence.id);
                g.node(Node {
                    id: pid.clone(),
                    kind: NodeKind::Person,
                    label: format!("person {}", evidence.id),
                    product: product.into(),
                    person: Some(evidence.id.clone()),
                    at_ms: None,
                    body: format!("person {}", evidence.id),
                });
                g.edge(&id, "affected", &pid);
            }
        }
    }

    for h in Hypothesis::all(lines.iter()) {
        let id = format!("hypothesis:{}", h.id.0);
        g.node(Node {
            id: id.clone(),
            kind: NodeKind::Hypothesis,
            label: format!("hypothesis {}", h.id.0),
            product: product.into(),
            person: None,
            at_ms: Some(h.proposed_at_ms),
            body: h.statement.clone(),
        });
        for cite in &h.cites {
            g.edge(&id, "cites", &format!("observation:{}", cite.0));
        }
        if let Some(plan) = &h.plan {
            let metric_id = format!("metric:{}", plan.metric.label());
            g.node(Node {
                id: metric_id.clone(),
                kind: NodeKind::Metric,
                label: plan.metric.label().into(),
                product: product.into(),
                person: None,
                at_ms: None,
                body: format!("metric {}", plan.metric.label()),
            });
            g.edge(&id, "expects_to_move", &metric_id);
        }
    }

    for b in ProductBet::all(lines.iter()) {
        let id = format!("bet:{}", b.id.0);
        let audience = b.audience.as_deref().map(|a| format!(" · for {a}")).unwrap_or_default();
        let alternatives = if b.alternatives.is_empty() {
            String::new()
        } else {
            format!(" · not taken: {}", b.alternatives.join("; "))
        };
        g.node(Node {
            id: id.clone(),
            kind: NodeKind::Bet,
            label: format!("bet {}", b.id.0),
            product: product.into(),
            person: None,
            at_ms: Some(b.created_at_ms),
            body: format!("{} · because {}{}{}", b.belief, b.rationale, audience, alternatives),
        });
        g.edge(&id, "wraps", &format!("hypothesis:{}", b.hypothesis.0));
        for (i, note) in ProductBet::learnings(lines.iter(), &b.id).into_iter().enumerate() {
            let lid = format!("learning:{}:{}", b.id.0, i);
            g.node(Node {
                id: lid.clone(),
                kind: NodeKind::Learning,
                label: format!("learning from {}", b.id.0),
                product: product.into(),
                person: None,
                at_ms: None,
                body: note,
            });
            g.edge(&lid, "taught_by", &id);
        }
    }

    for c in Change::all(lines.iter()) {
        let id = format!("change:{}", c.id.0);
        g.node(Node {
            id: id.clone(),
            kind: NodeKind::Change,
            label: format!("change {}", c.id.0),
            product: product.into(),
            person: None,
            at_ms: Some(c.shipped_at_ms),
            body: format!(
                "change {} at {} measured on {} over {}d",
                c.id.0,
                c.sha,
                c.plan.metric.label(),
                c.plan.window.days
            ),
        });
        g.edge(&id, "acts_on", &format!("hypothesis:{}", c.hypothesis.0));
        g.edge(&id, "shipped_as", &format!("deploy:{}", c.sha));
    }

    for (kind, value) in lines {
        if kind == OUTCOME_KIND {
            let Ok(rec) = serde_json::from_value::<OutcomeRecorded>(value.clone()) else { continue };
            let id = format!("outcome:{}", rec.change.0);
            let sentence = match &rec.outcome {
                engine_core::evaluation::Outcome::Measured { direction, from, to, entries, .. } => {
                    format!("{direction:?} {from} → {to} over {entries}").to_lowercase()
                }
                engine_core::evaluation::Outcome::Unmeasured(u) => u.sentence(),
            };
            g.node(Node {
                id: id.clone(),
                kind: NodeKind::Outcome,
                label: format!("outcome of {}", rec.change.0),
                product: product.into(),
                person: None,
                at_ms: Some(rec.measured_at_ms),
                body: sentence,
            });
            g.edge(&id, "measures", &format!("change:{}", rec.change.0));
        }
        // A bet's verdict enriches the bet node's searchable text — the
        // verdict is part of what the bet means once it exists.
        if kind == engine_core::bet::STATUS_KIND {
            if let Ok(s) = serde_json::from_value::<BetStatusChanged>(value.clone()) {
                if let BetStatus::Evaluated { verdict } = s.status {
                    let bid = format!("bet:{}", s.bet.0);
                    if let Some(node) = g.nodes.iter_mut().find(|n| n.id == bid) {
                        let word = match verdict.support {
                            engine_core::bet::Support::Supported => "supported",
                            engine_core::bet::Support::NotSupported => "not supported",
                            engine_core::bet::Support::Inconclusive => "inconclusive",
                        };
                        node.body.push_str(&format!(" · verdict {word}"));
                    }
                }
            }
        }
    }

    g
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::evaluation::Sample;
    use engine_core::hypothesis::HypothesisId;
    use engine_core::observation::{Kind, Source, Window};

    fn seeded() -> Vec<(String, serde_json::Value)> {
        let mut lines: Vec<(String, serde_json::Value)> = Vec::new();
        lines.push((
            "event".into(),
            serde_json::json!({
                "service": "api", "occurred_at_ms": 5,
                "error_name": "TypeError", "error_message": "boom",
                "stack": "TypeError: boom\n    at x (/s.js:1:1)"
            }),
        ));
        lines.push((
            "deploy".into(),
            serde_json::to_value(DeployRecord {
                sha: "abc1234def".into(),
                description: "ship the retry queue".into(),
                author: "sahil".into(),
                deployed_at_ms: 4,
            })
            .unwrap(),
        ));
        let o = Observation::fact(
            "obs_1",
            Source::Runtime,
            Kind::RateShift { previous: 0.1, since_deploy: Some("abc1234def".into()) },
            Window::new(0, 10).unwrap(),
            10,
        );
        lines.push(("observation".into(), serde_json::to_value(&o).unwrap()));
        let h = Hypothesis::new(
            "hyp_1",
            "the retry queue is retrying non-idempotent calls",
            vec![o.id.clone()],
            11,
        )
        .unwrap();
        lines.push(("hypothesis".into(), serde_json::to_value(&h).unwrap()));
        let b = ProductBet::new(
            "bet_1",
            "batching the retry queue will cut the error-event rate",
            "the rate tripled after abc1234def",
            HypothesisId("hyp_1".into()),
            12,
        )
        .unwrap();
        lines.push(("bet".into(), serde_json::to_value(&b).unwrap()));
        lines
    }

    #[test]
    fn the_graph_carries_the_chain_as_typed_edges() {
        let g = extract(&seeded(), "demo");
        let has = |src: &str, rel: &str, dst: &str| {
            g.edges.iter().any(|e| e.src == src && e.rel == rel && e.dst == dst)
        };
        assert!(has("error:api:TypeError", "occurred_in", "service:api"));
        assert!(has("observation:obs_1", "correlates_with", "deploy:abc1234def"));
        assert!(has("hypothesis:hyp_1", "cites", "observation:obs_1"));
        assert!(has("bet:bet_1", "wraps", "hypothesis:hyp_1"));
        assert!(g.nodes.iter().all(|n| n.product == "demo"), "every node is product-scoped");
    }

    #[test]
    fn extraction_is_deterministic_and_idempotent_over_duplicates() {
        let mut lines = seeded();
        let dup = lines.clone();
        lines.extend(dup);
        let once = extract(&seeded(), "demo");
        let twice = extract(&lines, "demo");
        assert_eq!(once.nodes.len(), twice.nodes.len(), "duplicate lines add nothing");
        assert_eq!(once.edges.len(), twice.edges.len());
    }

    #[test]
    fn bodies_are_quotes_from_the_record_not_summaries() {
        let g = extract(&seeded(), "demo");
        let bet = g.nodes.iter().find(|n| n.id == "bet:bet_1").unwrap();
        assert!(bet.body.contains("batching the retry queue"), "{}", bet.body);
        assert!(bet.body.contains("because the rate tripled"), "{}", bet.body);
        let err = g.nodes.iter().find(|n| n.id == "error:api:TypeError").unwrap();
        assert_eq!(err.body, "TypeError: boom");
    }

    #[test]
    fn a_person_in_evidence_becomes_a_scoped_node() {
        let mut lines = seeded();
        let o = Observation::reading(
            "obs_2",
            engine_core::evaluation::EvaluationId("eval_1".into()),
            Source::Support,
            engine_core::observation::Measure {
                metric: engine_core::evaluation::Metric::CompletionRate,
                sample: Sample { value: 0.5, entries: 100 },
            },
            Window::new(0, 10).unwrap(),
            10,
        )
        .with_evidence([engine_core::observation::EvidenceRef {
            source: Source::Support,
            kind: EvidenceKind::Person,
            id: "u_42".into(),
        }]);
        lines.push(("observation".into(), serde_json::to_value(&o).unwrap()));
        let g = extract(&lines, "demo");
        let person = g.nodes.iter().find(|n| n.id == "person:u_42").unwrap();
        assert_eq!(person.person.as_deref(), Some("u_42"), "person scope is carried");
        assert!(g.edges.iter().any(|e| e.src == "observation:obs_2" && e.rel == "affected"));
    }
}
