//! SQLite persistence for the semantic index.
//!
//! Ranking runs in Rust, not in SQL. The record is small (thousands of
//! lines, not millions), an in-process scan is instantaneous at that scale,
//! and keeping the scorer out of SQLite means no build of the bundled
//! library — FTS5 present or absent — can change what a query returns. When
//! a record someday outgrows this, the visible symptom is `drums ask`
//! latency, and the fix is an index, not a semantics change.

use rusqlite::{params, Connection};
use std::path::Path;

use crate::extract::{Graph, Node, NodeKind};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("semantic store: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("semantic store: embedding for {node} has {got} dims, store expects {expected}")]
    DimensionMismatch { node: String, got: usize, expected: usize },
}

/// One search result: the node, its score, and its one-hop context —
/// enough to read the hit without opening the record.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub node: Node,
    pub score: f64,
    /// `("rel", "label of the other node")`, both directions.
    pub context: Vec<(String, String)>,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if absent) the store at `path`. The schema is applied
    /// idempotently; there is no migration machinery because the store is
    /// rebuildable — a schema change ships as "delete and reindex".
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS nodes (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                label TEXT NOT NULL,
                product TEXT NOT NULL,
                person TEXT,
                at_ms INTEGER,
                body TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS edges (
                src TEXT NOT NULL,
                rel TEXT NOT NULL,
                dst TEXT NOT NULL,
                PRIMARY KEY (src, rel, dst)
            );
            CREATE TABLE IF NOT EXISTS embeddings (
                content_hash INTEGER NOT NULL,
                model TEXT NOT NULL,
                node_id TEXT NOT NULL,
                vec BLOB NOT NULL,
                PRIMARY KEY (content_hash, model)
            );
            CREATE TABLE IF NOT EXISTS index_state (
                k TEXT PRIMARY KEY,
                v TEXT NOT NULL
            );",
        )?;
        Ok(Store { conn })
    }

    /// Replace nodes and edges with the given graph. Embeddings survive —
    /// they are keyed by content hash, so unchanged text keeps its vector
    /// and only genuinely new text ever needs the embedder again.
    pub fn rebuild(&mut self, graph: &Graph, lines_indexed: usize) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM nodes", [])?;
        tx.execute("DELETE FROM edges", [])?;
        for n in &graph.nodes {
            tx.execute(
                "INSERT OR REPLACE INTO nodes (id, kind, label, product, person, at_ms, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    n.id,
                    n.kind.label(),
                    n.label,
                    n.product,
                    n.person,
                    n.at_ms.map(|v| v as i64),
                    n.body
                ],
            )?;
        }
        for e in &graph.edges {
            tx.execute(
                "INSERT OR REPLACE INTO edges (src, rel, dst) VALUES (?1, ?2, ?3)",
                params![e.src, e.rel, e.dst],
            )?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO index_state (k, v) VALUES ('lines_indexed', ?1)",
            params![lines_indexed.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// How many record lines the current index was built from. The engine's
    /// tick compares this against the live record to decide whether a
    /// rebuild is due — cheap, and exact for an append-only file.
    pub fn lines_indexed(&self) -> Result<Option<usize>, StoreError> {
        let mut stmt = self.conn.prepare("SELECT v FROM index_state WHERE k = 'lines_indexed'")?;
        let mut rows = stmt.query([])?;
        match rows.next()? {
            Some(row) => Ok(row.get::<_, String>(0)?.parse().ok()),
            None => Ok(None),
        }
    }

    pub fn counts(&self) -> Result<(usize, usize, usize), StoreError> {
        let nodes: i64 = self.conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))?;
        let edges: i64 = self.conn.query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))?;
        let vecs: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT node_id) FROM embeddings",
            [],
            |r| r.get(0),
        )?;
        Ok((nodes as usize, edges as usize, vecs as usize))
    }

    fn all_nodes(&self) -> Result<Vec<Node>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, kind, label, product, person, at_ms, body FROM nodes")?;
        let rows = stmt.query_map([], |row| {
            Ok(Node {
                id: row.get(0)?,
                kind: kind_from(&row.get::<_, String>(1)?),
                label: row.get(2)?,
                product: row.get(3)?,
                person: row.get(4)?,
                at_ms: row.get::<_, Option<i64>>(5)?.map(|v| v as u64),
                body: row.get(6)?,
            })
        })?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    fn context_for(&self, id: &str) -> Result<Vec<(String, String)>, StoreError> {
        let mut out = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT e.rel, n.label FROM edges e JOIN nodes n ON n.id = e.dst WHERE e.src = ?1",
        )?;
        let rows = stmt.query_map(params![id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        out.extend(rows.filter_map(Result::ok));
        let mut stmt = self.conn.prepare(
            "SELECT e.rel, n.label FROM edges e JOIN nodes n ON n.id = e.src WHERE e.dst = ?1",
        )?;
        let rows = stmt.query_map(params![id], |r| {
            Ok((format!("{} ←", r.get::<_, String>(0)?), r.get::<_, String>(1)?))
        })?;
        out.extend(rows.filter_map(Result::ok));
        Ok(out)
    }

    /// Token-overlap lexical ranking with length normalization. Deliberately
    /// simple: at record scale, the difference between this and BM25 is not
    /// measurable, and this is inspectable by reading twelve lines.
    pub fn search_lexical(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, StoreError> {
        let terms: Vec<String> = tokens(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let mut scored: Vec<(f64, Node)> = Vec::new();
        for node in self.all_nodes()? {
            let hay = tokens(&format!("{} {}", node.label, node.body));
            if hay.is_empty() {
                continue;
            }
            let matched = terms.iter().filter(|t| hay.contains(t)).count();
            if matched == 0 {
                continue;
            }
            let score = matched as f64 / terms.len() as f64
                + matched as f64 / (hay.len() as f64).sqrt() * 0.1;
            scored.push((score, node));
        }
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored
            .into_iter()
            .take(limit)
            .map(|(score, node)| {
                let context = self.context_for(&node.id)?;
                Ok(SearchHit { node, score, context })
            })
            .collect()
    }

    /// Store an embedding for a node's current body. Keyed by content hash +
    /// model: a rebuild that does not change the text never re-embeds.
    pub fn put_embedding(
        &mut self,
        node_id: &str,
        content_hash: u64,
        model: &str,
        vec: &[f32],
    ) -> Result<(), StoreError> {
        let bytes: Vec<u8> = vec.iter().flat_map(|f| f.to_le_bytes()).collect();
        self.conn.execute(
            "INSERT OR REPLACE INTO embeddings (content_hash, model, node_id, vec)
             VALUES (?1, ?2, ?3, ?4)",
            params![content_hash as i64, model, node_id, bytes],
        )?;
        Ok(())
    }

    /// Nodes whose current body has no embedding under `model` — the work
    /// list for `embed_missing`, returned as (id, body, hash).
    pub fn unembedded(&self, model: &str) -> Result<Vec<(String, String, u64)>, StoreError> {
        let mut out = Vec::new();
        for node in self.all_nodes()? {
            let hash = crate::content_hash(&node.body);
            let found: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM embeddings WHERE content_hash = ?1 AND model = ?2",
                params![hash as i64, model],
                |r| r.get(0),
            )?;
            if found == 0 {
                out.push((node.id, node.body, hash));
            }
        }
        Ok(out)
    }

    /// Brute-force cosine over stored vectors for `model`. Vectors whose
    /// content hash no longer matches any live node are skipped — stale
    /// cache entries must not resurrect deleted text into results.
    pub fn search_vector(
        &self,
        model: &str,
        query: &[f32],
        limit: usize,
    ) -> Result<Vec<SearchHit>, StoreError> {
        let nodes = self.all_nodes()?;
        let mut scored: Vec<(f64, Node)> = Vec::new();
        let mut stmt = self
            .conn
            .prepare("SELECT vec FROM embeddings WHERE content_hash = ?1 AND model = ?2")?;
        for node in nodes {
            let hash = crate::content_hash(&node.body) as i64;
            let bytes: Option<Vec<u8>> =
                stmt.query_row(params![hash, model], |r| r.get(0)).ok();
            let Some(bytes) = bytes else { continue };
            let vec: Vec<f32> = bytes
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            if vec.len() != query.len() {
                return Err(StoreError::DimensionMismatch {
                    node: node.id,
                    got: vec.len(),
                    expected: query.len(),
                });
            }
            scored.push((cosine(query, &vec), node));
        }
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored
            .into_iter()
            .take(limit)
            .map(|(score, node)| {
                let context = self.context_for(&node.id)?;
                Ok(SearchHit { node, score, context })
            })
            .collect()
    }
}

fn tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| t.len() > 1)
        .map(|t| t.to_string())
        .collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| (*x as f64) * (*y as f64)).sum();
    let na: f64 = a.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    let nb: f64 = b.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

fn kind_from(label: &str) -> NodeKind {
    match label {
        "service" => NodeKind::Service,
        "error" => NodeKind::ErrorSignature,
        "deploy" => NodeKind::Deploy,
        "person" => NodeKind::Person,
        "metric" => NodeKind::Metric,
        "hypothesis" => NodeKind::Hypothesis,
        "bet" => NodeKind::Bet,
        "change" => NodeKind::Change,
        "outcome" => NodeKind::Outcome,
        "learning" => NodeKind::Learning,
        _ => NodeKind::Observation,
    }
}

/// Update the store from the record: rebuild nodes and edges when the line
/// count moved, leave everything untouched when it did not. Returns whether
/// a rebuild ran. The is-it-due check is a count comparison because the
/// record is append-only — lines never mutate in place, so "same count"
/// means "same content" for everything already indexed.
pub fn refresh(
    store: &mut Store,
    lines: &[(String, serde_json::Value)],
    product: &str,
) -> Result<bool, StoreError> {
    if store.lines_indexed()? == Some(lines.len()) {
        return Ok(false);
    }
    let graph = crate::extract(lines, product);
    store.rebuild(&graph, lines.len())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{Edge, Graph};

    fn node(id: &str, body: &str) -> Node {
        Node {
            id: id.into(),
            kind: NodeKind::Bet,
            label: id.into(),
            product: "demo".into(),
            person: None,
            at_ms: Some(1),
            body: body.into(),
        }
    }

    fn store_with(nodes: Vec<Node>, edges: Vec<Edge>) -> Store {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(&dir.path().join("semantic.db")).unwrap();
        s.rebuild(&Graph { nodes, edges }, 3).unwrap();
        // Leak the tempdir so the db outlives this constructor in tests.
        std::mem::forget(dir);
        s
    }

    #[test]
    fn lexical_search_finds_by_meaning_bearing_words_with_context() {
        let s = store_with(
            vec![
                node("bet:1", "batching the retry queue will cut the error-event rate"),
                node("bet:2", "a progress bar will raise onboarding completion"),
            ],
            vec![Edge { src: "bet:1".into(), rel: "wraps", dst: "bet:2".into() }],
        );
        let hits = s.search_lexical("retry queue errors", 5).unwrap();
        assert_eq!(hits[0].node.id, "bet:1");
        assert!(hits[0].context.iter().any(|(rel, _)| rel == "wraps"), "one-hop context rides along");
        assert!(s.search_lexical("kubernetes", 5).unwrap().is_empty(), "no match is empty, not noise");
    }

    #[test]
    fn a_rebuild_keeps_embeddings_for_unchanged_text() {
        let mut s = store_with(vec![node("bet:1", "same text")], vec![]);
        let hash = crate::content_hash("same text");
        s.put_embedding("bet:1", hash, "test-model", &[1.0, 0.0]).unwrap();
        assert!(s.unembedded("test-model").unwrap().is_empty());
        // Rebuild with identical text: still embedded, nothing to re-pay.
        s.rebuild(&Graph { nodes: vec![node("bet:1", "same text")], edges: vec![] }, 4).unwrap();
        assert!(s.unembedded("test-model").unwrap().is_empty());
        // Text changed: the old vector no longer applies and the node is due.
        s.rebuild(&Graph { nodes: vec![node("bet:1", "different text")], edges: vec![] }, 5).unwrap();
        assert_eq!(s.unembedded("test-model").unwrap().len(), 1);
    }

    #[test]
    fn vector_search_ranks_by_cosine_and_skips_stale_vectors() {
        let mut s = store_with(
            vec![node("bet:1", "alpha"), node("bet:2", "beta"), node("bet:3", "gamma")],
            vec![],
        );
        s.put_embedding("bet:1", crate::content_hash("alpha"), "m", &[1.0, 0.0]).unwrap();
        s.put_embedding("bet:2", crate::content_hash("beta"), "m", &[0.0, 1.0]).unwrap();
        // bet:3 has no embedding — it is absent from vector results, not zero-scored.
        let hits = s.search_vector("m", &[1.0, 0.1], 5).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].node.id, "bet:1");
    }

    #[test]
    fn dimension_mismatch_is_an_error_not_a_silent_zero() {
        let mut s = store_with(vec![node("bet:1", "alpha")], vec![]);
        s.put_embedding("bet:1", crate::content_hash("alpha"), "m", &[1.0, 0.0, 0.0]).unwrap();
        let err = s.search_vector("m", &[1.0, 0.0], 5).unwrap_err();
        assert!(matches!(err, StoreError::DimensionMismatch { .. }), "mixed models must be loud");
    }

    #[test]
    fn refresh_rebuilds_only_when_the_record_grew() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(&dir.path().join("semantic.db")).unwrap();
        let lines = vec![(
            "deploy".into(),
            serde_json::to_value(engine_core::DeployRecord {
                sha: "abc".into(),
                description: "d".into(),
                author: "a".into(),
                deployed_at_ms: 1,
            })
            .unwrap(),
        )];
        assert!(refresh(&mut s, &lines, "demo").unwrap(), "first refresh builds");
        assert!(!refresh(&mut s, &lines, "demo").unwrap(), "same count is a no-op");
        let (nodes, _, _) = s.counts().unwrap();
        assert_eq!(nodes, 1);
    }
}
