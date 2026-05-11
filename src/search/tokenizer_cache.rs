//! Fast-path tokenizer cache.
//!
//! `Tokenizer::from_file` parses the model's 1 MB tokenizer.json on every
//! warm hybrid/semantic search ~ ~75-85 ms of pure CPU spent rebuilding a
//! WordPiece vocabulary + BertNormalizer + BertPreTokenizer that already
//! lived in this process moments ago in a sibling invocation. The cost is
//! dominated by serde's per-string allocation across ~60 k vocab entries.
//!
//! This module persists the bare minimum the warm path needs (vocab + Bert
//! normalizer flags + the two added-tokens stamps from BERT-style models)
//! to a packed binary file, then rebuilds an equivalent `Tokenizer` via
//! the public `WordPiece::builder()` / `Tokenizer::new` API. The cached
//! tokenizer covers the encode path (`encode_batch_fast` for one query
//! per `search` invocation) ~ the decode path, truncation params, and
//! padding params are not stamped because the warm encode path doesn't
//! consult them.
//!
//! Cache invalidation is by (mtime_ns, size) of the source tokenizer.json,
//! same trick as `model_files_meta`. A mismatch falls back to the slow
//! JSON parse and overwrites the cache on the way out.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use ahash::AHashMap;

use tokenizers::models::wordpiece::WordPiece;
use tokenizers::models::ModelWrapper;
use tokenizers::normalizers::bert::BertNormalizer;
use tokenizers::normalizers::NormalizerWrapper;
use tokenizers::pre_tokenizers::bert::BertPreTokenizer;
use tokenizers::pre_tokenizers::PreTokenizerWrapper;
use tokenizers::tokenizer::{AddedToken, Tokenizer};
use tokenizers::Model;

use crate::cache::cache_root;

/// Cache format identifier ~ bumped when the layout changes shape.
const MAGIC: u32 = 0xB0CA_C0DE;
const VERSION: u32 = 1;
/// Versioned subdir under hitagi's cache root. Lives outside `<repo_hash>/`
/// because tokenizers are shared across every repo that uses the same
/// embedding model.
const SUBDIR: &str = "tokenizer_meta.v1";

#[derive(Debug)]
struct AddedTokenMeta {
    id: u32,
    content: String,
    flags: u8,
}

const FLAG_SINGLE_WORD: u8 = 1 << 0;
const FLAG_LSTRIP: u8 = 1 << 1;
const FLAG_RSTRIP: u8 = 1 << 2;
const FLAG_NORMALIZED: u8 = 1 << 3;
const FLAG_SPECIAL: u8 = 1 << 4;

/// Where the cache for `tokenizer_path` would live. Returns `None` when no
/// cache root is resolvable (no HOME / no XDG_CACHE_HOME).
fn cache_path_for(tokenizer_path: &Path) -> Option<PathBuf> {
    let root = cache_root()?;
    let hash = path_hash(tokenizer_path);
    Some(root.join(SUBDIR).join(format!("{hash}.bin")))
}

/// Deterministic short hash of an absolute path. Filenames stay ASCII and
/// short regardless of the original tokenizer.json location.
fn path_hash(path: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Result of a warm cache hit. Precomputes the small bits the model2vec
/// encoder would otherwise re-derive (median token byte length and unk
/// token id) so the calling thread doesn't have to redo `get_vocab()` /
/// `token_to_id()` against the rebuilt `Tokenizer` on every search.
pub struct CachedTokenizer {
    pub tokenizer: Tokenizer,
    pub median_token_length: usize,
    pub unk_token_id: Option<u32>,
}

/// Read the cached tokenizer if its (mtime, size) match the on-disk
/// `tokenizer_path`. Any decode failure or stat mismatch returns `None` so
/// the caller falls through to the slow JSON path.
pub fn try_load(tokenizer_path: &Path) -> Option<CachedTokenizer> {
    let cache_path = cache_path_for(tokenizer_path)?;
    // mmap rather than `fs::read` ~ we walk the 1 MB blob exactly once
    // (the decoder iterates the vocab into an owned `AHashMap`), so
    // borrowing the OS page cache via `&[u8]` skips the userspace
    // memcpy that `fs::read` allocates a fresh `Vec<u8>` for. The
    // mapping doesn't need to outlive this call ~ all owned data lives
    // in the rebuilt `Tokenizer`.
    let file = fs::File::open(&cache_path).ok()?;
    // Safety: the cache file is written via atomic temp+rename in
    // `write()` so the mapping never observes a partial write.
    let mmap = unsafe { memmap2::Mmap::map(&file) }.ok()?;
    decode(&mmap[..], tokenizer_path).ok()
}

/// Serialize `tokenizer` into the cache file for `tokenizer_path`. Best
/// effort ~ a failure to write never breaks the encoder; the next warm
/// search just pays the JSON parse again. `median_token_length` and
/// `unk_token_id` are computed once on the slow path and stamped into the
/// cache so warm runs can read them straight back without iterating the
/// vocab a second time.
pub fn write(
    tokenizer_path: &Path,
    tokenizer: &Tokenizer,
    median_token_length: usize,
    unk_token_id: Option<u32>,
) {
    let Some(cache_path) = cache_path_for(tokenizer_path) else {
        return;
    };
    let Some(bytes) = encode(tokenizer, tokenizer_path, median_token_length, unk_token_id) else {
        return;
    };
    if let Some(parent) = cache_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // Atomic temp+rename so a partial write can't poison the next reader.
    let tmp = cache_path.with_extension("bin.tmp");
    if let Ok(mut file) = fs::File::create(&tmp) {
        if file.write_all(&bytes).is_ok() {
            let _ = file.flush();
            drop(file);
            let _ = fs::rename(&tmp, &cache_path);
        }
    }
}

fn stat_tuple(path: &Path) -> Option<(i128, u64)> {
    let md = fs::metadata(path).ok()?;
    let mtime_ns = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128)?;
    Some((mtime_ns, md.len()))
}

fn encode(
    tokenizer: &Tokenizer,
    tokenizer_path: &Path,
    median_token_length: usize,
    unk_token_id: Option<u32>,
) -> Option<Vec<u8>> {
    // Down-cast to the model we actually support. Anything else (BPE,
    // Unigram, WordLevel) silently skips the cache ~ the slow path stays
    // identical, just slower.
    let (vocab, unk_token, subword_prefix, max_chars) = wordpiece_state(tokenizer)?;
    let normalizer = bert_normalizer_flags(tokenizer);
    if !has_bert_pre_tokenizer(tokenizer) {
        return None;
    }
    let added = added_tokens_meta(tokenizer);
    let (mtime_ns, size) = stat_tuple(tokenizer_path)?;

    let mut out: Vec<u8> = Vec::with_capacity(2 * 1024 * 1024);
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&mtime_ns.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&(median_token_length as u32).to_le_bytes());
    let (unk_present, unk_id) = match unk_token_id {
        Some(id) => (1u8, id),
        None => (0u8, 0u32),
    };
    out.push(unk_present);
    out.extend_from_slice(&unk_id.to_le_bytes());
    out.extend_from_slice(&(vocab.len() as u32).to_le_bytes());
    for (token, id) in &vocab {
        let bytes = token.as_bytes();
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(bytes);
        out.extend_from_slice(&id.to_le_bytes());
    }
    write_lp_string(&mut out, &unk_token);
    write_lp_string(&mut out, &subword_prefix);
    out.extend_from_slice(&max_chars.to_le_bytes());
    let (clean_text, handle_chinese, strip_accents, lowercase) = normalizer;
    let flag_byte = (clean_text as u8) | ((handle_chinese as u8) << 1) | ((lowercase as u8) << 2);
    out.push(flag_byte);
    let strip_byte: u8 = match strip_accents {
        None => 0,
        Some(true) => 1,
        Some(false) => 2,
    };
    out.push(strip_byte);
    out.extend_from_slice(&(added.len() as u32).to_le_bytes());
    for at in &added {
        out.extend_from_slice(&at.id.to_le_bytes());
        write_lp_string(&mut out, &at.content);
        out.push(at.flags);
    }
    Some(out)
}

fn decode(bytes: &[u8], tokenizer_path: &Path) -> Result<CachedTokenizer, &'static str> {
    let mut cursor = ByteReader::new(bytes);
    let magic = cursor.read_u32()?;
    if magic != MAGIC {
        return Err("tokenizer cache magic mismatch");
    }
    let version = cursor.read_u32()?;
    if version != VERSION {
        return Err("tokenizer cache version mismatch");
    }
    let mtime_ns = cursor.read_i128()?;
    let size = cursor.read_u64()?;
    let stat = stat_tuple(tokenizer_path).ok_or("tokenizer stat failed")?;
    if stat.0 != mtime_ns || stat.1 != size {
        return Err("tokenizer source changed since cache write");
    }
    let median_token_length = cursor.read_u32()? as usize;
    let unk_present = cursor.read_u8()?;
    let unk_id_raw = cursor.read_u32()?;
    let unk_token_id = (unk_present != 0).then_some(unk_id_raw);
    let vocab_count = cursor.read_u32()? as usize;
    let mut vocab_map: AHashMap<String, u32> = AHashMap::with_capacity(vocab_count);
    for _ in 0..vocab_count {
        let token = cursor.read_lp_str()?.to_owned();
        let id = cursor.read_u32()?;
        vocab_map.insert(token, id);
    }
    let unk_token = cursor.read_lp_str()?.to_owned();
    let subword_prefix = cursor.read_lp_str()?.to_owned();
    let max_chars = cursor.read_u32()? as usize;
    let flag_byte = cursor.read_u8()?;
    let clean_text = (flag_byte & 1) != 0;
    let handle_chinese = (flag_byte & 2) != 0;
    let lowercase = (flag_byte & 4) != 0;
    let strip_byte = cursor.read_u8()?;
    let strip_accents = match strip_byte {
        0 => None,
        1 => Some(true),
        2 => Some(false),
        _ => return Err("tokenizer cache: bad strip_accents marker"),
    };
    let added_count = cursor.read_u32()? as usize;
    let mut added: Vec<AddedToken> = Vec::with_capacity(added_count);
    for _ in 0..added_count {
        let _id = cursor.read_u32()?;
        let content = cursor.read_lp_str()?.to_owned();
        let f = cursor.read_u8()?;
        let token = AddedToken::from(content, (f & FLAG_SPECIAL) != 0)
            .single_word((f & FLAG_SINGLE_WORD) != 0)
            .lstrip((f & FLAG_LSTRIP) != 0)
            .rstrip((f & FLAG_RSTRIP) != 0)
            .normalized((f & FLAG_NORMALIZED) != 0);
        added.push(token);
    }

    let wp = WordPiece::builder()
        .vocab(vocab_map)
        .unk_token(unk_token)
        .continuing_subword_prefix(subword_prefix)
        .max_input_chars_per_word(max_chars)
        .build()
        .map_err(|_| "wordpiece build failed")?;
    let mut tokenizer = Tokenizer::new(wp);
    tokenizer.with_normalizer(Some(BertNormalizer::new(
        clean_text,
        handle_chinese,
        strip_accents,
        lowercase,
    )));
    tokenizer.with_pre_tokenizer(Some(BertPreTokenizer));
    if !added.is_empty() {
        // Skipping `add_special_tokens` produces tiny score deltas on
        // queries that don't contain the literal special-token strings
        // (the AddedVocabulary AhoCorasick index is consulted regardless
        // of whether the input mentions one). We keep them so the cached
        // path reproduces the JSON path bit-for-bit; the cost is one
        // O(special_tokens) call, negligible against the vocab build.
        let _ = tokenizer.add_tokens(&added);
    }
    Ok(CachedTokenizer {
        tokenizer,
        median_token_length,
        unk_token_id,
    })
}

fn wordpiece_state(tokenizer: &Tokenizer) -> Option<(Vec<(String, u32)>, String, String, u32)> {
    let model = tokenizer.get_model();
    let ModelWrapper::WordPiece(wp) = model else {
        return None;
    };
    let vocab: Vec<(String, u32)> = wp.get_vocab().into_iter().collect();
    let unk = wp.unk_token.clone();
    let prefix = wp.continuing_subword_prefix.clone();
    let max_chars = wp.max_input_chars_per_word as u32;
    Some((vocab, unk, prefix, max_chars))
}

fn bert_normalizer_flags(tokenizer: &Tokenizer) -> (bool, bool, Option<bool>, bool) {
    let Some(n) = tokenizer.get_normalizer() else {
        return (false, false, None, false);
    };
    if let NormalizerWrapper::BertNormalizer(b) = n {
        return (
            b.clean_text,
            b.handle_chinese_chars,
            b.strip_accents,
            b.lowercase,
        );
    }
    (false, false, None, false)
}

fn has_bert_pre_tokenizer(tokenizer: &Tokenizer) -> bool {
    matches!(
        tokenizer.get_pre_tokenizer(),
        Some(PreTokenizerWrapper::BertPreTokenizer(_))
    )
}

fn added_tokens_meta(tokenizer: &Tokenizer) -> Vec<AddedTokenMeta> {
    let added = tokenizer.get_added_vocabulary();
    let mut out: Vec<AddedTokenMeta> = Vec::new();
    // The crate doesn't expose an iterator over AddedTokens directly; the
    // `get_added_tokens_decoder` map is the closest public surface and
    // yields the AddedToken structs we need to rebuild the same shape on
    // the load side.
    for (id, token) in added.get_added_tokens_decoder() {
        let mut flags = 0u8;
        if token.single_word {
            flags |= FLAG_SINGLE_WORD;
        }
        if token.lstrip {
            flags |= FLAG_LSTRIP;
        }
        if token.rstrip {
            flags |= FLAG_RSTRIP;
        }
        if token.normalized {
            flags |= FLAG_NORMALIZED;
        }
        if token.special {
            flags |= FLAG_SPECIAL;
        }
        out.push(AddedTokenMeta {
            id: *id,
            content: token.content.clone(),
            flags,
        });
    }
    // Sort by id so the cache layout is deterministic across runs even
    // though the source map is hash-ordered.
    out.sort_by_key(|a| a.id);
    out
}

fn write_lp_string(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

struct ByteReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn read_slice(&mut self, n: usize) -> Result<&'a [u8], &'static str> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or("tokenizer cache: read overflow")?;
        if end > self.bytes.len() {
            return Err("tokenizer cache: truncated");
        }
        let s = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn read_u8(&mut self) -> Result<u8, &'static str> {
        Ok(self.read_slice(1)?[0])
    }

    fn read_u32(&mut self) -> Result<u32, &'static str> {
        let s = self.read_slice(4)?;
        Ok(u32::from_le_bytes(s.try_into().unwrap()))
    }

    fn read_u64(&mut self) -> Result<u64, &'static str> {
        let s = self.read_slice(8)?;
        Ok(u64::from_le_bytes(s.try_into().unwrap()))
    }

    fn read_i128(&mut self) -> Result<i128, &'static str> {
        let s = self.read_slice(16)?;
        Ok(i128::from_le_bytes(s.try_into().unwrap()))
    }

    fn read_lp_str(&mut self) -> Result<&'a str, &'static str> {
        let n = self.read_u32()? as usize;
        let bytes = self.read_slice(n)?;
        std::str::from_utf8(bytes).map_err(|_| "tokenizer cache: bad utf8")
    }
}
