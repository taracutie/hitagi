//! BM25 sparse index.
//!
//! Tokenizes each chunk's content + path stem + last 3 parent dir parts so
//! a query for `session` matches `src/auth/session.rs` even when the chunk
//! body never mentions the word. BM25 with the canonical Robertson
//! parameters (k1=1.5, b=0.75); query terms deduplicated before scoring so
//! repeated keywords don't bloat the score.
//!
//! On-disk layout is a flat, allocation-light layout (see `Bm25Index` field
//! docs): four parallel `Vec`s plus a single byte buffer for term strings.
//! Decode is a handful of memcpys via bincode's slice paths rather than
//! O(N) HashMap inserts, which was where ~30 ms of warm-search time used to
//! go on the web repo (~70 k unique terms).

use std::collections::HashSet;
use std::path::Path;

use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use super::ranking::truncate_top_k;
use super::tokens::tokenize;
use super::types::IndexedChunk;

/// One posting: a (doc_id, term_frequency) pair. Packed `repr(C)` so the
/// `postings` Vec is a contiguous byte buffer that bincode can copy in one
/// memcpy on decode.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[repr(C)]
pub struct Posting {
    pub doc_id: u32,
    pub tf: u32,
}

/// Flat BM25 index. Terms live in a single concatenated byte buffer with a
/// parallel offsets vector; the per-term posting slice is delimited by
/// `posting_offsets`. Search does a binary lookup on the sorted term table
/// once per query token instead of a HashMap probe, then slices straight
/// into `postings`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bm25Index {
    /// Concatenated UTF-8 bytes of every term in `term_offsets` order.
    term_bytes: Vec<u8>,
    /// `term_offsets[i]..term_offsets[i+1]` is term i's byte range inside
    /// `term_bytes`. Length is `term_count + 1` (sentinel).
    term_offsets: Vec<u32>,
    /// `posting_offsets[i]..posting_offsets[i+1]` is term i's posting range
    /// inside `postings`. Length is `term_count + 1`.
    posting_offsets: Vec<u32>,
    /// All postings, grouped by term. Within a term, sorted by `doc_id`.
    postings: Vec<Posting>,
    /// Per-doc token counts; `doc_id` indexes here.
    doc_len: Vec<u32>,
    avg_doc_len: f32,
    doc_count: u32,
}

impl Bm25Index {
    pub fn build_from_chunks(chunks: &[IndexedChunk]) -> Self {
        Self::build_from_tokenized_docs(chunks.iter().map(tokens_for_chunk), chunks.len())
    }

    fn build_from_tokenized_docs(
        docs: impl Iterator<Item = Vec<String>>,
        doc_count: usize,
    ) -> Self {
        // Aggregate (term -> Vec<(doc_id, tf)>) in a temporary FxHashMap. We
        // throw it away after flattening into the sorted-by-term layout; the
        // persisted form holds parallel Vecs instead of a HashMap so decode
        // skips the O(N) hash inserts that used to dominate `load_sparse`.
        let mut postings_map: FxHashMap<String, Vec<(u32, u32)>> = FxHashMap::default();
        let mut doc_len: Vec<u32> = Vec::with_capacity(doc_count);
        for (doc_id, tokens) in docs.enumerate() {
            doc_len.push(tokens.len() as u32);
            let mut counts: FxHashMap<String, u32> = FxHashMap::default();
            for token in tokens {
                *counts.entry(token).or_default() += 1;
            }
            for (token, tf) in counts {
                postings_map
                    .entry(token)
                    .or_default()
                    .push((doc_id as u32, tf));
            }
        }

        // Sort terms so search can binary-search the term table.
        let mut sorted_terms: Vec<(String, Vec<(u32, u32)>)> = postings_map.into_iter().collect();
        sorted_terms.sort_by(|a, b| a.0.cmp(&b.0));

        let term_count = sorted_terms.len();
        let mut term_bytes: Vec<u8> = Vec::new();
        let mut term_offsets: Vec<u32> = Vec::with_capacity(term_count + 1);
        let mut posting_offsets: Vec<u32> = Vec::with_capacity(term_count + 1);
        let mut postings: Vec<Posting> = Vec::new();
        for (term, mut docs_for_term) in sorted_terms {
            term_offsets.push(term_bytes.len() as u32);
            term_bytes.extend_from_slice(term.as_bytes());
            posting_offsets.push(postings.len() as u32);
            docs_for_term.sort_unstable_by_key(|(doc_id, _)| *doc_id);
            postings.extend(docs_for_term.into_iter().map(|(doc_id, tf)| Posting { doc_id, tf }));
        }
        term_offsets.push(term_bytes.len() as u32);
        posting_offsets.push(postings.len() as u32);

        let avg_doc_len = if doc_len.is_empty() {
            0.0
        } else {
            doc_len.iter().map(|&n| n as u64).sum::<u64>() as f32 / doc_len.len() as f32
        };
        Self {
            term_bytes,
            term_offsets,
            posting_offsets,
            postings,
            doc_len,
            avg_doc_len,
            doc_count: doc_count as u32,
        }
    }

    /// Resolve a term string to its index in the term table via binary search.
    /// `None` when the term isn't present. Called once per unique query token
    /// at search time; the sorted layout means we never build a HashMap on
    /// load.
    fn find_term(&self, query: &[u8]) -> Option<usize> {
        let term_count = self.term_offsets.len().saturating_sub(1);
        let mut lo = 0usize;
        let mut hi = term_count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let start = self.term_offsets[mid] as usize;
            let end = self.term_offsets[mid + 1] as usize;
            let term = &self.term_bytes[start..end];
            match term.cmp(query) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    fn postings_for(&self, term_idx: usize) -> &[Posting] {
        let start = self.posting_offsets[term_idx] as usize;
        let end = self.posting_offsets[term_idx + 1] as usize;
        &self.postings[start..end]
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
        let allowed: Option<HashSet<u32>> =
            selector.map(|s| s.iter().map(|&id| id as u32).collect());
        let mut scores: FxHashMap<u32, f32> = FxHashMap::default();
        let unique_terms: HashSet<String> = tokens.into_iter().collect();
        let doc_count_f = self.doc_count as f32;
        let avg = self.avg_doc_len.max(1.0);
        let k1 = 1.5f32;
        let b = 0.75f32;
        for term in unique_terms {
            let Some(term_idx) = self.find_term(term.as_bytes()) else {
                continue;
            };
            let postings = self.postings_for(term_idx);
            let df = postings.len() as f32;
            let idf = ((doc_count_f - df + 0.5) / (df + 0.5) + 1.0).ln();
            for &Posting { doc_id, tf } in postings {
                if allowed.as_ref().is_some_and(|set| !set.contains(&doc_id)) {
                    continue;
                }
                let tf = tf as f32;
                let dl = self.doc_len[doc_id as usize] as f32;
                let denom = tf + k1 * (1.0 - b + b * dl / avg);
                let score = idf * (tf * (k1 + 1.0)) / denom;
                *scores.entry(doc_id).or_default() += score;
            }
        }
        let mut ranked: Vec<(usize, f32)> = scores
            .into_iter()
            .filter(|(_, score)| *score > 0.0)
            .map(|(id, score)| (id as usize, score))
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
