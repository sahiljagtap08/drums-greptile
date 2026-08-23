//! The semantic index: the record, readable by meaning.
//!
//! # What this is, and what it is deliberately not
//!
//! Every object Drums records — events, deploys, observations, hypotheses,
//! bets, changes, outcomes, learnings — becomes a node in a typed graph with
//! searchable text, scoped to the product and (where one exists) to an opaque
//! person. `drums ask` answers questions over it; the drafting agent reads it
//! for context; the memory surfaces render from it.
//!
//! It is an **index, never a second source of truth**. The append-only record
//! is authoritative; `semantic.db` is derived from it deterministically and
//! can be deleted at any time — `drums index` rebuilds it byte-for-byte
//! equivalent. Nothing is ever written here that is not in the record, which
//! is what keeps the audit story intact: there is exactly one thing to trust.
//!
//! # Graph and vector, honestly scoped
//!
//! The graph (nodes + typed edges) and lexical search work always, with zero
//! configuration — ranking runs in Rust over the store, so no SQLite
//! extension availability can change behavior between machines. Vector
//! search needs an embedding source, and Drums never possesses model
//! credentials of its own ([agent-neutrality]), so embeddings run through an
//! OpenAI-compatible endpoint the customer configures by environment —
//! `DRUMS_EMBED_URL`, `DRUMS_EMBED_MODEL`, `DRUMS_EMBED_API_KEY` (key
//! env-only, never a config file, same discipline as PostHog). Without one,
//! search says plainly that it ran lexical-only. Embeddings are cached by
//! content hash so a rebuild never re-pays for unchanged text.
//!
//! [agent-neutrality]: the customer's model, the customer's key, or nothing.
//!
//! # For whoever builds cross-source clustering on top of this
//!
//! Do not embed raw signals. Off-the-shelf embedding models cluster on
//! structural similarity — every stack trace near every stack trace, every
//! chat message near every chat message — so an error about checkout and a
//! support message about checkout never meet. (PostHog shipped this lesson
//! after building it wrong once.) And do not replace it with one generated
//! query per signal, which trades bad embedding similarity for bad
//! query-generation similarity. The pipeline that holds up:
//! source-specific normalization → a generated semantic description
//! (intent, affected surface, symptom, suspected user goal, entities, a
//! few retrieval queries) → metadata filters → semantic retrieval → a
//! candidate cluster → **an agent verifying the candidates are actually
//! the same phenomenon**. Verification stays downstream, always: retrieval
//! proposes, it never concludes.

pub mod embed;
pub mod extract;
pub mod store;

pub use embed::{embedder_from_env, EmbedError, Embedder, MISSING_EMBED_ENV};
pub use extract::{extract, Edge, Graph, Node, NodeKind};
pub use store::{SearchHit, Store, StoreError};

/// One answer to `drums ask`: ranked hits, each carrying enough graph
/// context to be read without opening the record, plus the honest statement
/// of how the search ran.
#[derive(Debug)]
pub struct Answer {
    pub hits: Vec<SearchHit>,
    /// `true` when a vector pass contributed to ranking. `false` means
    /// lexical-only — said out loud by the CLI, never silently degraded.
    pub semantic: bool,
}

/// Search the index. Lexical always; vector when an embedder is provided and
/// the store holds embeddings for its model.
pub async fn ask(
    store: &Store,
    query: &str,
    embedder: Option<&dyn Embedder>,
    limit: usize,
) -> Result<Answer, StoreError> {
    let lexical = store.search_lexical(query, limit.max(8))?;
    let Some(embedder) = embedder else {
        return Ok(Answer { hits: truncate(lexical, limit), semantic: false });
    };
    let query_vec = match embedder.embed(&[query.to_string()]).await {
        Ok(mut vs) if !vs.is_empty() => vs.remove(0),
        // An embedder that fails at query time must not fail the question:
        // fall back to lexical and say so via `semantic: false`.
        _ => return Ok(Answer { hits: truncate(lexical, limit), semantic: false }),
    };
    let vector = store.search_vector(embedder.model(), &query_vec, limit.max(8))?;
    if vector.is_empty() {
        return Ok(Answer { hits: truncate(lexical, limit), semantic: false });
    }
    Ok(Answer { hits: merge(lexical, vector, limit), semantic: true })
}

fn truncate(mut hits: Vec<SearchHit>, limit: usize) -> Vec<SearchHit> {
    hits.truncate(limit);
    hits
}

/// Reciprocal-rank fusion. Chosen over score mixing because lexical and
/// cosine scores live on incomparable scales, and normalizing them invents a
/// comparability that is not there; rank positions are comparable by
/// construction.
fn merge(lexical: Vec<SearchHit>, vector: Vec<SearchHit>, limit: usize) -> Vec<SearchHit> {
    const K: f64 = 60.0;
    let mut fused: std::collections::HashMap<String, (f64, SearchHit)> =
        std::collections::HashMap::new();
    for list in [lexical, vector] {
        for (rank, hit) in list.into_iter().enumerate() {
            let add = 1.0 / (K + rank as f64 + 1.0);
            fused
                .entry(hit.node.id.clone())
                .and_modify(|(s, _)| *s += add)
                .or_insert((add, hit));
        }
    }
    let mut out: Vec<(f64, SearchHit)> = fused.into_values().collect();
    out.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    out.into_iter().map(|(_, h)| h).take(limit).collect()
}

/// FNV-1a over text — the content hash that keys the embedding cache.
/// Deliberately not `DefaultHasher`, whose keys are randomized per process:
/// a cache keyed on it would silently miss on every restart and re-pay for
/// every embedding.
pub fn content_hash(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_is_stable_across_processes_by_construction() {
        // Pinned value: if this changes, every cached embedding silently
        // invalidates on upgrade, which is a cost bug, not a correctness bug —
        // still worth a deliberate decision rather than an accident.
        assert_eq!(content_hash(""), 0xcbf29ce484222325);
        assert_eq!(content_hash("drums"), content_hash("drums"));
        assert_ne!(content_hash("drums"), content_hash("drum"));
    }

    #[test]
    fn fusion_prefers_agreement_over_either_list_alone() {
        let hit = |id: &str| SearchHit {
            node: Node {
                id: id.into(),
                kind: NodeKind::Observation,
                label: id.into(),
                product: "p".into(),
                person: None,
                at_ms: Some(1),
                body: id.into(),
            },
            score: 1.0,
            context: vec![],
        };
        // "b" is ranked second in both lists; "a" and "c" each lead one list.
        let lexical = vec![hit("a"), hit("b")];
        let vector = vec![hit("c"), hit("b")];
        let merged = merge(lexical, vector, 3);
        assert_eq!(merged[0].node.id, "b", "agreement across both passes outranks a single first place");
        assert_eq!(merged.len(), 3);
    }
}
