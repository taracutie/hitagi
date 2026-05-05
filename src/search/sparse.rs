//! BM25 sparse index.
//!
//! Tokenizes each chunk's content + path stem + last 3 parent dir parts so
//! a query for `session` matches `src/auth/session.rs` even when the chunk
//! body never mentions the word. BM25 with the canonical Robertson
//! parameters (k1=1.5, b=0.75); query terms deduplicated before scoring so
//! repeated keywords don't bloat the score.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::ranking::truncate_top_k;
use super::tokens::tokenize;
use super::types::IndexedChunk;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bm25Index {
    postings: HashMap<String, Vec<(usize, u32)>>,
    doc_len: Vec<usize>,
    avg_doc_len: f32,
    doc_count: usize,
}

impl Bm25Index {
    pub fn build_from_chunks(chunks: &[IndexedChunk]) -> Self {
        Self::build_from_tokenized_docs(chunks.iter().map(tokens_for_chunk), chunks.len())
    }

    fn build_from_tokenized_docs(
        docs: impl Iterator<Item = Vec<String>>,
        doc_count: usize,
    ) -> Self {
        let mut postings_map: HashMap<String, HashMap<usize, u32>> = HashMap::new();
        let mut doc_len = Vec::with_capacity(doc_count);
        for (doc_id, tokens) in docs.enumerate() {
            doc_len.push(tokens.len());
            let mut counts: HashMap<String, u32> = HashMap::new();
            for token in tokens {
                *counts.entry(token).or_default() += 1;
            }
            for (token, tf) in counts {
                postings_map.entry(token).or_default().insert(doc_id, tf);
            }
        }
        let postings = postings_map
            .into_iter()
            .map(|(term, docs)| {
                let mut docs: Vec<(usize, u32)> = docs.into_iter().collect();
                docs.sort_unstable_by_key(|(doc_id, _)| *doc_id);
                (term, docs)
            })
            .collect();
        let avg_doc_len = if doc_len.is_empty() {
            0.0
        } else {
            doc_len.iter().sum::<usize>() as f32 / doc_len.len() as f32
        };
        Self {
            postings,
            doc_len,
            avg_doc_len,
            doc_count,
        }
    }

    pub fn search(
        &self,
        query: &str,
        top_k: usize,
        selector: Option<&[usize]>,
    ) -> Vec<(usize, f32)> {
        let tokens = tokenize(query);
        if tokens.is_empty() || top_k == 0 {
            return Vec::new();
        }
        if selector.is_some_and(|s| s.is_empty()) {
            return Vec::new();
        }
        let allowed: Option<HashSet<usize>> = selector.map(|s| s.iter().copied().collect());
        let mut scores: HashMap<usize, f32> = HashMap::new();
        let unique_terms: HashSet<String> = tokens.into_iter().collect();
        for term in unique_terms {
            let Some(postings) = self.postings.get(&term) else {
                continue;
            };
            let df = postings.len() as f32;
            let idf = ((self.doc_count as f32 - df + 0.5) / (df + 0.5) + 1.0).ln();
            for &(doc_id, tf) in postings {
                if allowed.as_ref().is_some_and(|set| !set.contains(&doc_id)) {
                    continue;
                }
                let tf = tf as f32;
                let dl = self.doc_len[doc_id] as f32;
                let k1 = 1.5f32;
                let b = 0.75f32;
                let denom = tf + k1 * (1.0 - b + b * dl / self.avg_doc_len.max(1.0));
                let score = idf * (tf * (k1 + 1.0)) / denom;
                *scores.entry(doc_id).or_default() += score;
            }
        }
        let mut ranked: Vec<(usize, f32)> = scores
            .into_iter()
            .filter(|(_, score)| *score > 0.0)
            .collect();
        truncate_top_k(&mut ranked, top_k);
        ranked
    }
}

fn tokens_for_chunk(chunk: &IndexedChunk) -> Vec<String> {
    let mut tokens = tokenize(&chunk.content);
    let path = Path::new(&chunk.file_path);
    if let Some(stem) = path.file_stem().map(|s| s.to_string_lossy()) {
        let stem_tokens = tokenize(&stem);
        // Doubled so the stem outweighs a single-mention body token ~ search
        // for "session" should rank session.rs above a chart.rs that
        // happens to mention "session" once in passing.
        tokens.extend(stem_tokens.iter().cloned());
        tokens.extend(stem_tokens);
    }
    if let Some(parent) = path.parent() {
        let parts = parent
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .filter(|part| part != "." && part != "/")
            .collect::<Vec<_>>();
        for part in parts
            .iter()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            tokens.extend(tokenize(part));
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::Bm25Index;
    use crate::search::types::IndexedChunk;

    fn chunk(content: &str, file_path: &str) -> IndexedChunk {
        IndexedChunk {
            content: content.to_owned(),
            file_path: file_path.to_owned(),
            start_line: 1,
            end_line: 1,
            language: Some("rust".to_owned()),
        }
    }

    #[test]
    fn bm25_search_uses_content_and_path_tokens() {
        let chunks = vec![
            chunk("fn parse_token() {}", "src/auth/session.rs"),
            chunk("fn render_view() {}", "src/ui/view.rs"),
        ];
        let index = Bm25Index::build_from_chunks(&chunks);

        let results = index.search("session", 1, None);

        assert_eq!(results[0].0, 0);
    }

    #[test]
    fn bm25_search_respects_selector() {
        let chunks = vec![chunk("alpha token", "a.rs"), chunk("alpha token", "b.rs")];
        let index = Bm25Index::build_from_chunks(&chunks);
        let results = index.search("alpha", 10, Some(&[1]));
        assert_eq!(results, vec![(1, results[0].1)]);
    }

    #[test]
    fn bm25_search_with_empty_selector_returns_no_candidates() {
        let chunks = vec![chunk("alpha token", "a.rs"), chunk("alpha token", "b.rs")];
        let index = Bm25Index::build_from_chunks(&chunks);
        let results = index.search("alpha", 10, Some(&[]));
        assert!(results.is_empty());
    }
}
