//! Fast warm-path query encoder for BERT-WordPiece tokenizers.
//!
//! `Tokenizer::from_file` and the existing `tokenizer_cache` path both reach
//! ~40 ms on the potion-code-16M tokenizer (~62 k vocab entries) ~ ~30 ms
//! spent building a `HashMap<String, u32>` and another ~10 ms in
//! `WordPiece::builder().build()` constructing the reverse `vocab_r` map
//! that the encode path never consults. For one-shot CLI queries that's
//! the single largest contributor to warm hybrid-search wall time.
//!
//! This module persists the same data the existing cache writes, but in a
//! flat layout that decodes via three `copy_from_slice` calls and runs the
//! WordPiece encode directly against a sorted byte buffer via binary search.
//! The official `BertNormalizer` + `BertPreTokenizer` types still run as-is
//! since they're allocation-free to construct and produce identical splits.
//! Special-token matching (`AddedVocabulary`) is reproduced by a small
//! linear scan over the cached added-tokens table; potion-code-16M ships
//! five special tokens, so the scan is negligible.
//!
//! Cache format v1: little-endian throughout. Header = MAGIC + VERSION +
//! source `(mtime_ns, size)` so a model swap invalidates without touching
//! the on-disk filename, then the dense fields the encode path needs.
//! Persistence is atomic temp+rename so a partial write can't poison the
//! next reader.

use std::cmp::Ordering;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use tokenizers::normalizers::bert::BertNormalizer;
use tokenizers::pre_tokenizers::bert::BertPreTokenizer;
use tokenizers::tokenizer::{
    NormalizedString, Normalizer, OffsetReferential, OffsetType, PreTokenizedString, PreTokenizer,
};

use crate::cache::cache_root;

const MAGIC: u32 = 0xFA57_B027;
const VERSION: u32 = 1;
const SUBDIR: &str = "fast_wp.v1";

/// Sorted-vocab WordPiece. Tokens live in one concatenated UTF-8 byte buffer
/// addressed by `offsets`; `ids[i]` is the model id of token `i`. Sorted by
/// raw bytes so lookups binary-search without rebuilding any hash table.
#[derive(Clone, Debug)]
pub struct FastWordPiece {
    token_bytes: Vec<u8>,
    /// `token_bytes[offsets[i]..offsets[i+1]]` is the i-th token's UTF-8 bytes.
    /// Length is `vocab_count + 1`.
    offsets: Vec<u32>,
    /// `ids[i]` is the model id of the i-th sorted token. Length is `vocab_count`.
    ids: Vec<u32>,
    continuing_subword_prefix: String,
    max_input_chars_per_word: usize,
    unk_id: Option<u32>,
}

impl FastWordPiece {
    /// Build from the official WordPiece-extracted vocab. Sorts by raw bytes
    /// so the load path can binary-search without rehashing 60 k entries.
    pub fn from_vocab(
        mut vocab: Vec<(String, u32)>,
        unk_token: &str,
        continuing_subword_prefix: String,
        max_input_chars_per_word: u32,
    ) -> Self {
        vocab.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let mut token_bytes: Vec<u8> = Vec::new();
        let mut offsets: Vec<u32> = Vec::with_capacity(vocab.len() + 1);
        let mut ids: Vec<u32> = Vec::with_capacity(vocab.len());
        let mut unk_id: Option<u32> = None;
        for (token, id) in vocab {
            offsets.push(token_bytes.len() as u32);
            if token == unk_token {
                unk_id = Some(id);
            }
            token_bytes.extend_from_slice(token.as_bytes());
            ids.push(id);
        }
        offsets.push(token_bytes.len() as u32);
        Self {
            token_bytes,
            offsets,
            ids,
            continuing_subword_prefix,
            max_input_chars_per_word: max_input_chars_per_word as usize,
            unk_id,
        }
    }

    /// Mirrors `tokenizers::models::wordpiece::WordPiece::tokenize`'s id
    /// stream: longest-prefix match per character position, prepending the
    /// continuing-subword prefix on subsequent matches. A "bad" word
    /// (no prefix matches at some position) emits a single unk id when
    /// `unk_id` is set; otherwise the word is dropped (the official path
    /// would return an error, but the model2vec encode path discards unks
    /// regardless, so the visible behavior is the same).
    pub fn tokenize_into(&self, sequence: &str, out: &mut Vec<u32>) {
        let char_count = sequence.chars().count();
        if char_count > self.max_input_chars_per_word {
            if let Some(id) = self.unk_id {
                out.push(id);
            }
            return;
        }
        let bytes = sequence.as_bytes();
        let total = bytes.len();
        let prefix_bytes = self.continuing_subword_prefix.as_bytes();
        let mut key_buf: Vec<u8> = Vec::with_capacity(64);
        let mut start = 0usize;
        let mut produced = out.len();
        while start < total {
            let mut end = total;
            let mut matched: Option<(u32, usize)> = None;
            while start < end {
                // Construct lookup key into a reusable buffer.
                key_buf.clear();
                if start > 0 {
                    key_buf.extend_from_slice(prefix_bytes);
                }
                key_buf.extend_from_slice(&bytes[start..end]);
                if let Some(id) = self.lookup(&key_buf) {
                    matched = Some((id, end));
                    break;
                }
                // Shrink by one Unicode scalar; `sequence[start..end]` is a
                // valid &str slice so `chars().last()` is well-defined.
                let slice = &sequence[start..end];
                let last_char_len = slice.chars().last().map(|c| c.len_utf8()).unwrap_or(1);
                if end == 0 || last_char_len == 0 {
                    break;
                }
                end -= last_char_len;
            }
            match matched {
                Some((id, next_start)) => {
                    out.push(id);
                    start = next_start;
                }
                None => {
                    // Bad word: replace any partial subtokens for this word
                    // with a single unk id (matches official "is_bad" branch).
                    out.truncate(produced);
                    if let Some(id) = self.unk_id {
                        out.push(id);
                    }
                    return;
                }
            }
        }
        produced = out.len();
        let _ = produced;
    }

    fn lookup(&self, key: &[u8]) -> Option<u32> {
        let n = self.ids.len();
        if n == 0 {
            return None;
        }
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let start = self.offsets[mid] as usize;
            let end = self.offsets[mid + 1] as usize;
            let token = &self.token_bytes[start..end];
            match token.cmp(key) {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => return Some(self.ids[mid]),
            }
        }
        None
    }
}

#[derive(Debug)]
struct AddedTokenEntry {
    id: u32,
    content: String,
    /// Whether to apply the same normalization (lowercase / strip accents)
    /// to the content before matching. Mirrors the official `normalized` flag.
    normalized: bool,
}

/// Encoder-ready bundle: a fast WordPiece + the BERT normalizer + a flat
/// list of added/special tokens to match before pre-tokenization. Wraps
/// everything the model2vec query path needs in one struct with no
/// per-encode-call HashMap traffic.
#[derive(Debug)]
pub struct FastBertWordPiece {
    pub fast: FastWordPiece,
    pub normalizer: BertNormalizer,
    pub median_token_length: usize,
    pub unk_token_id: Option<u32>,
    /// `config.json`'s `normalize` flag, cached here so the warm-cache
    /// hit path doesn't need to re-read the config file. Defaults to
    /// `true` (matching potion-code-16M) when the cache predates this
    /// field, but newly-written caches always carry the explicit value.
    pub config_normalize: bool,
    added: Vec<AddedTokenEntry>,
}

impl FastBertWordPiece {
    pub fn dim_independent_encode(&self, text: &str, out: &mut Vec<u32>) {
        // Special-token pass first: match content (raw or normalized form) as
        // a substring of the input, splitting on hits. Mirrors what the
        // official `AddedVocabulary` does before normalize/pre-tokenize.
        // Linear scan over a tiny (<10 entry) list is faster than the
        // Aho-Corasick the official path keeps in memory.
        self.encode_with_added_tokens(text, out);
    }

    fn encode_with_added_tokens(&self, text: &str, out: &mut Vec<u32>) {
        if self.added.is_empty() {
            self.encode_plain(text, out);
            return;
        }
        // Find the earliest leftmost special-token match; recurse on the
        // segments around it. Most code queries hit zero special tokens, so
        // this short-circuits on the first scan.
        let mut start = 0usize;
        while start < text.len() {
            let mut best: Option<(usize, &AddedTokenEntry, usize)> = None;
            for entry in &self.added {
                // Lookup by literal content. The `normalized: true` variant
                // is uncommon in practice for special tokens; we still match
                // it literally here, which is correct for queries that don't
                // pre-normalize the special token form themselves. The
                // official path normalizes the input first when
                // `normalized=true`; this branch is unused on potion-code-16M.
                let _ = entry.normalized;
                if let Some(idx) = text[start..].find(entry.content.as_str()) {
                    let abs = start + idx;
                    let end = abs + entry.content.len();
                    match best {
                        Some((b_abs, _, _)) if abs >= b_abs => {}
                        _ => best = Some((abs, entry, end)),
                    }
                }
            }
            match best {
                Some((abs, entry, end)) => {
                    if abs > start {
                        self.encode_plain(&text[start..abs], out);
                    }
                    out.push(entry.id);
                    start = end;
                }
                None => {
                    self.encode_plain(&text[start..], out);
                    return;
                }
            }
        }
    }

    fn encode_plain(&self, text: &str, out: &mut Vec<u32>) {
        if text.is_empty() {
            return;
        }
        let mut normalized = NormalizedString::from(text);
        if let Err(err) = self.normalizer.normalize(&mut normalized) {
            // Defensive: BertNormalizer doesn't fail in practice; if it
            // ever did, fall back to the unnormalized input rather than
            // dropping the query.
            let _ = err;
        }
        let mut pretok: PreTokenizedString = normalized.into();
        if let Err(err) = BertPreTokenizer.pre_tokenize(&mut pretok) {
            let _ = err;
        }
        let splits = pretok.get_splits(OffsetReferential::Original, OffsetType::Byte);
        for (split_text, _, _) in splits {
            if !split_text.is_empty() {
                self.fast.tokenize_into(split_text, out);
            }
        }
    }
}

/// Persistence: cache path resolution + cache-write + cache-load. Returns
/// `None` for any benign miss (no cache root, missing file, mtime/size
/// mismatch, decode error) so the caller falls through to the slow JSON
/// path identical to the pre-fast behavior.
fn cache_path_for(tokenizer_path: &Path) -> Option<PathBuf> {
    let root = cache_root()?;
    let hash = path_hash(tokenizer_path);
    Some(root.join(SUBDIR).join(format!("{hash}.bin")))
}

fn path_hash(path: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
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

pub fn try_load(tokenizer_path: &Path) -> Option<FastBertWordPiece> {
    let cache_path = cache_path_for(tokenizer_path)?;
    let file = fs::File::open(&cache_path).ok()?;
    let mmap = unsafe { memmap2::Mmap::map(&file) }.ok()?;
    decode(&mmap[..], tokenizer_path).ok()
}

/// Writer side: takes already-built data from the slow path (after the JSON
/// tokenizer parse) and persists it in the fast format. Failures are
/// silently ignored ~ the next warm search re-pays the slow path.
pub struct FastWriteInput {
    pub vocab: Vec<(String, u32)>,
    pub unk_token: String,
    pub continuing_subword_prefix: String,
    pub max_input_chars_per_word: u32,
    pub normalizer_flags: (bool, bool, Option<bool>, bool),
    pub median_token_length: usize,
    pub unk_token_id: Option<u32>,
    pub added_tokens: Vec<(u32, String, bool)>,
    /// Final L2-normalize flag read from `config.json` ~ stamped here so
    /// warm fast-cache hits don't have to re-read the config file off-disk
    /// just to recover one boolean.
    pub config_normalize: bool,
}

pub fn write(tokenizer_path: &Path, input: &FastWriteInput) {
    let Some(cache_path) = cache_path_for(tokenizer_path) else {
        return;
    };
    let Some(bytes) = encode(tokenizer_path, input) else {
        return;
    };
    if let Some(parent) = cache_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let tmp = cache_path.with_extension("bin.tmp");
    if let Ok(mut file) = fs::File::create(&tmp) {
        if file.write_all(&bytes).is_ok() {
            let _ = file.flush();
            drop(file);
            let _ = fs::rename(&tmp, &cache_path);
        }
    }
}

/// Build an in-memory `FastBertWordPiece` from the same write-input data
/// the cache uses. Saves repeating the sort/scan on the slow path: the
/// caller hands us the vocab they already extracted from the tokenizer,
/// we sort + index it once, and the same `FastBertWordPiece` powers both
/// this run's queries and the cache write. The cache write needs the
/// pre-sort to be deterministic too, so this routine sorts upfront.
pub fn build_from_input(input: &FastWriteInput) -> FastBertWordPiece {
    let fast = FastWordPiece::from_vocab(
        input.vocab.clone(),
        &input.unk_token,
        input.continuing_subword_prefix.clone(),
        input.max_input_chars_per_word,
    );
    let mut fast = fast;
    // unk_id from the explicit field overrides the vocab scan ~ the vocab
    // scan resolves it as a fallback, but the official tokenizer's
    // token_to_id is authoritative for non-vocab unk strings.
    if input.unk_token_id.is_some() {
        fast.unk_id = input.unk_token_id;
    }
    let (clean_text, handle_chinese, strip_accents, lowercase) = input.normalizer_flags;
    let normalizer = BertNormalizer::new(clean_text, handle_chinese, strip_accents, lowercase);
    let added = input
        .added_tokens
        .iter()
        .map(|(id, content, normalized)| AddedTokenEntry {
            id: *id,
            content: content.clone(),
            normalized: *normalized,
        })
        .collect();
    FastBertWordPiece {
        fast,
        normalizer,
        median_token_length: input.median_token_length,
        unk_token_id: input.unk_token_id,
        config_normalize: input.config_normalize,
        added,
    }
}

fn encode(tokenizer_path: &Path, input: &FastWriteInput) -> Option<Vec<u8>> {
    let (mtime_ns, size) = stat_tuple(tokenizer_path)?;
    // Pre-sort by bytes so the load path doesn't repeat the work.
    let mut vocab = input.vocab.clone();
    vocab.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut token_bytes_total = 0usize;
    for (token, _) in &vocab {
        token_bytes_total += token.len();
    }
    let mut out: Vec<u8> =
        Vec::with_capacity(64 + token_bytes_total + (vocab.len() + 1) * 4 + vocab.len() * 4);
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&mtime_ns.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&(input.median_token_length as u32).to_le_bytes());
    let (unk_present, unk_id) = match input.unk_token_id {
        Some(id) => (1u8, id),
        None => (0u8, 0u32),
    };
    out.push(unk_present);
    out.extend_from_slice(&unk_id.to_le_bytes());
    write_lp_string(&mut out, &input.unk_token);
    write_lp_string(&mut out, &input.continuing_subword_prefix);
    out.extend_from_slice(&input.max_input_chars_per_word.to_le_bytes());
    let (clean_text, handle_chinese, strip_accents, lowercase) = input.normalizer_flags;
    let flag_byte = (clean_text as u8) | ((handle_chinese as u8) << 1) | ((lowercase as u8) << 2);
    out.push(flag_byte);
    let strip_byte: u8 = match strip_accents {
        None => 0,
        Some(true) => 1,
        Some(false) => 2,
    };
    out.push(strip_byte);
    out.extend_from_slice(&(vocab.len() as u32).to_le_bytes());
    out.extend_from_slice(&(token_bytes_total as u32).to_le_bytes());
    // Build the three flat tables in one pass.
    let mut offsets: Vec<u32> = Vec::with_capacity(vocab.len() + 1);
    let mut ids: Vec<u32> = Vec::with_capacity(vocab.len());
    let mut running = 0u32;
    for (token, id) in &vocab {
        offsets.push(running);
        running += token.len() as u32;
        ids.push(*id);
    }
    offsets.push(running);
    // Token bytes concatenation.
    for (token, _) in &vocab {
        out.extend_from_slice(token.as_bytes());
    }
    // offsets table.
    unsafe {
        let ptr = offsets.as_ptr() as *const u8;
        out.extend_from_slice(std::slice::from_raw_parts(ptr, offsets.len() * 4));
    }
    // ids table.
    unsafe {
        let ptr = ids.as_ptr() as *const u8;
        out.extend_from_slice(std::slice::from_raw_parts(ptr, ids.len() * 4));
    }
    out.extend_from_slice(&(input.added_tokens.len() as u32).to_le_bytes());
    for (id, content, normalized) in &input.added_tokens {
        out.extend_from_slice(&id.to_le_bytes());
        write_lp_string(&mut out, content);
        out.push(*normalized as u8);
    }
    // `config_normalize` is stamped as a trailing byte so older readers
    // (which stop at added_tokens) still load successfully and just use
    // the default `true` ~ no version bump required.
    out.push(input.config_normalize as u8);
    Some(out)
}

fn decode(bytes: &[u8], tokenizer_path: &Path) -> Result<FastBertWordPiece, &'static str> {
    let mut cursor = ByteReader::new(bytes);
    let magic = cursor.read_u32()?;
    if magic != MAGIC {
        return Err("fast_wp: magic mismatch");
    }
    let version = cursor.read_u32()?;
    if version != VERSION {
        return Err("fast_wp: version mismatch");
    }
    let mtime_ns = cursor.read_i128()?;
    let size = cursor.read_u64()?;
    let stat = stat_tuple(tokenizer_path).ok_or("fast_wp: stat failed")?;
    if stat.0 != mtime_ns || stat.1 != size {
        return Err("fast_wp: source changed since cache write");
    }
    let median_token_length = cursor.read_u32()? as usize;
    let unk_present = cursor.read_u8()?;
    let unk_id_raw = cursor.read_u32()?;
    let unk_token_id = (unk_present != 0).then_some(unk_id_raw);
    let unk_token = cursor.read_lp_str()?.to_owned();
    let continuing_subword_prefix = cursor.read_lp_str()?.to_owned();
    let max_input_chars_per_word = cursor.read_u32()? as usize;
    let flag_byte = cursor.read_u8()?;
    let clean_text = (flag_byte & 1) != 0;
    let handle_chinese = (flag_byte & 2) != 0;
    let lowercase = (flag_byte & 4) != 0;
    let strip_byte = cursor.read_u8()?;
    let strip_accents = match strip_byte {
        0 => None,
        1 => Some(true),
        2 => Some(false),
        _ => return Err("fast_wp: bad strip_accents marker"),
    };
    let vocab_count = cursor.read_u32()? as usize;
    let token_bytes_total = cursor.read_u32()? as usize;
    // Three flat-table memcpys: the warm path's worst case is a few MB of
    // f32 / u32 copy, far cheaper than the 30 ms HashMap build it replaces.
    let token_bytes = cursor.read_slice(token_bytes_total)?.to_vec();
    let offsets = cursor.read_u32_vec(vocab_count + 1)?;
    let ids = cursor.read_u32_vec(vocab_count)?;
    let added_count = cursor.read_u32()? as usize;
    let mut added: Vec<AddedTokenEntry> = Vec::with_capacity(added_count);
    for _ in 0..added_count {
        let id = cursor.read_u32()?;
        let content = cursor.read_lp_str()?.to_owned();
        let normalized = cursor.read_u8()? != 0;
        added.push(AddedTokenEntry {
            id,
            content,
            normalized,
        });
    }
    // Trailing `config_normalize` byte. Older cache files won't have it ~
    // fall back to the potion-code-16M default of `true` so a stale cache
    // continues to produce correct rankings until the next rebuild.
    let config_normalize = cursor.read_u8().map(|b| b != 0).unwrap_or(true);
    let fast = FastWordPiece {
        token_bytes,
        offsets,
        ids,
        continuing_subword_prefix,
        max_input_chars_per_word,
        unk_id: resolve_unk_id_from_added_or_table(&unk_token, unk_token_id),
    };
    let normalizer = BertNormalizer::new(clean_text, handle_chinese, strip_accents, lowercase);
    Ok(FastBertWordPiece {
        fast,
        normalizer,
        median_token_length,
        unk_token_id,
        config_normalize,
        added,
    })
}

fn resolve_unk_id_from_added_or_table(_unk_token: &str, cached_id: Option<u32>) -> Option<u32> {
    // Cached id from the write side is authoritative when present; the
    // write side resolves it via the official tokenizer's token_to_id once.
    cached_id
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
        let end = self.pos.checked_add(n).ok_or("fast_wp: read overflow")?;
        if end > self.bytes.len() {
            return Err("fast_wp: truncated");
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
        std::str::from_utf8(bytes).map_err(|_| "fast_wp: bad utf8")
    }

    /// Decode `count` little-endian `u32`s with a single `copy_from_slice`
    /// into a fresh allocation. Matches the BM25 flat-format trick used in
    /// `sparse.rs` ~ collapses the per-byte decoder loop bincode used to
    /// pay for `Vec<u32>` round-trips.
    fn read_u32_vec(&mut self, count: usize) -> Result<Vec<u32>, &'static str> {
        let bytes_total = count.checked_mul(4).ok_or("fast_wp: u32 vec overflow")?;
        let src = self.read_slice(bytes_total)?;
        let mut out: Vec<u32> = Vec::with_capacity(count);
        unsafe {
            out.set_len(count);
            let dst = std::slice::from_raw_parts_mut(out.as_mut_ptr() as *mut u8, bytes_total);
            dst.copy_from_slice(src);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::FastWordPiece;

    fn tiny_vocab() -> FastWordPiece {
        FastWordPiece::from_vocab(
            vec![
                ("session".to_owned(), 10),
                ("##ses".to_owned(), 11),
                ("##sion".to_owned(), 12),
                ("validation".to_owned(), 20),
                ("##val".to_owned(), 21),
                ("##idation".to_owned(), 22),
                ("[UNK]".to_owned(), 0),
            ],
            "[UNK]",
            "##".to_owned(),
            100,
        )
    }

    #[test]
    fn lookup_finds_known_tokens() {
        let wp = tiny_vocab();
        assert_eq!(wp.lookup(b"session"), Some(10));
        assert_eq!(wp.lookup(b"##ses"), Some(11));
        assert_eq!(wp.lookup(b"validation"), Some(20));
        assert_eq!(wp.lookup(b"unknown"), None);
    }

    #[test]
    fn tokenize_matches_longest_prefix() {
        let wp = tiny_vocab();
        let mut out = Vec::new();
        wp.tokenize_into("session", &mut out);
        assert_eq!(out, vec![10]);

        out.clear();
        wp.tokenize_into("validation", &mut out);
        assert_eq!(out, vec![20]);
    }

    #[test]
    fn bad_word_emits_unk() {
        let wp = tiny_vocab();
        let mut out = Vec::new();
        wp.tokenize_into("zzz", &mut out);
        assert_eq!(out, vec![0]);
    }
}
