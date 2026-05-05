//! Ranked code search ~ tree-sitter chunking + BM25 sparse + Model2Vec dense
//! + RRF hybrid. Replaces the literal-substring `search` command.
//!
//! The index lives alongside the parse cache in the same per-repo SQLite
//! file (`$XDG_CACHE_HOME/hitagi/<repo>/index.v6.sqlite`). Cold runs walk
//! the repo, chunk pack-supported files, build BM25 postings and (for
//! hybrid/semantic) Model2Vec embeddings, and persist the result. Warm runs
//! deserialize the persisted blobs and run the search in ~100ms.

pub mod chunker;
pub mod dense;
pub mod engine;
pub mod fuse;
pub mod model2vec;
pub mod model_cache;
pub mod persist;
pub mod ranking;
pub mod sparse;
pub mod tokens;
pub mod types;
pub mod walker;
