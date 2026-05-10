//! Flat columnar storage for the indexed chunk corpus.
//!
//! Ranking iterates the full chunk vector (e.g. `boost_multi_chunk_files`
//! looks at every chunk's `file_path`), but only the top-K hits ever need
//! their `content` materialized into an owned `String`. The previous
//! `Vec<IndexedChunk>` round-tripped one `String` allocation per field per
//! chunk through bincode on every warm `load_sparse` ~ on the web repo
//! that's ~100k tiny heap allocations for ~22 ms of decode + ~20 MB of RSS.
//!
//! `ChunkStore` keeps content in one contiguous byte buffer and addresses
//! it by offset table; file paths and language labels are interned into
//! small `Vec<Box<str>>` tables so chunks that share a file share the same
//! allocation. Accessors return `&str` slices into the buffer; the only
//! place that allocates owned strings is `to_indexed`, called O(top_k)
//! times per search instead of O(corpus).
//!
//! Persistence uses a hand-rolled binary format with fixed-size little-
//! endian integers; bincode's varint encoding would re-pay ~6 varints per
//! chunk × 32k chunks = ~200k per-byte decoder loops on every warm
//! `load_sparse`, which was empirically slower than the in-place vector
//! approach it was meant to replace. The flat format here decodes via a
//! handful of `memcpy`-shaped reads.

use std::collections::HashMap;

use super::types::IndexedChunk;

/// `lang_idx` sentinel for chunks with no detected language.
const LANG_NONE: i32 = -1;
/// On-disk format tag for the chunk-store blob. Bump only when the byte
/// layout changes shape; field-only schema changes (new column, etc.) ride
/// the same tag and rely on the size prefix.
const STORE_FORMAT_TAG: u32 = 0xC07E_5701;
const CHUNK_RECORD_BYTES: usize = std::mem::size_of::<ChunkRecord>();

/// One per chunk; 24 bytes packed. `path_idx`/`lang_idx` index into the
/// interned `paths` / `languages` tables; `content_start`/`content_end`
/// slice into the shared `content_bytes` buffer.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ChunkRecord {
    pub content_start: u32,
    pub content_end: u32,
    pub path_idx: u32,
    pub lang_idx: i32,
    pub start_line: u32,
    pub end_line: u32,
}

/// Columnar storage for an indexed chunk corpus.
#[derive(Clone, Debug, Default)]
pub struct ChunkStore {
    records: Vec<ChunkRecord>,
    paths: Vec<Box<str>>,
    languages: Vec<Box<str>>,
    /// All chunk content concatenated, UTF-8. Each chunk's slice is
    /// `content_bytes[content_start..content_end]`. UTF-8 validity is
    /// guaranteed at build time (every byte comes from a `String` field
    /// of an `IndexedChunk`), so accessors return `&str` via
    /// `from_utf8_unchecked` to skip a per-read validation pass.
    content_bytes: Vec<u8>,
}

impl ChunkStore {
    pub fn from_indexed(chunks: Vec<IndexedChunk>) -> Self {
        if chunks.is_empty() {
            return Self::default();
        }
        let total_content: usize = chunks.iter().map(|c| c.content.len()).sum();
        let mut records: Vec<ChunkRecord> = Vec::with_capacity(chunks.len());
        let mut paths: Vec<Box<str>> = Vec::new();
        let mut path_index: HashMap<String, u32> = HashMap::new();
        let mut languages: Vec<Box<str>> = Vec::new();
        let mut lang_index: HashMap<String, i32> = HashMap::new();
        let mut content_bytes: Vec<u8> = Vec::with_capacity(total_content);
        for chunk in chunks {
            let content_start = content_bytes.len() as u32;
            content_bytes.extend_from_slice(chunk.content.as_bytes());
            let content_end = content_bytes.len() as u32;
            let path_idx = *path_index.entry(chunk.file_path.clone()).or_insert_with(|| {
                let idx = paths.len() as u32;
                paths.push(chunk.file_path.clone().into_boxed_str());
                idx
            });
            let lang_idx = match chunk.language {
                Some(lang) => *lang_index.entry(lang.clone()).or_insert_with(|| {
                    let idx = languages.len() as i32;
                    languages.push(lang.into_boxed_str());
                    idx
                }),
                None => LANG_NONE,
            };
            records.push(ChunkRecord {
                content_start,
                content_end,
                path_idx,
                lang_idx,
                start_line: chunk.start_line as u32,
                end_line: chunk.end_line as u32,
            });
        }
        Self {
            records,
            paths,
            languages,
            content_bytes,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    #[inline]
    pub fn file_path(&self, id: usize) -> &str {
        &self.paths[self.records[id].path_idx as usize]
    }

    #[inline]
    pub fn content(&self, id: usize) -> &str {
        let r = &self.records[id];
        let bytes = &self.content_bytes[r.content_start as usize..r.content_end as usize];
        // Safety: every byte sequence originated from a `String` field of an
        // `IndexedChunk`, so the buffer is well-formed UTF-8 by construction.
        unsafe { std::str::from_utf8_unchecked(bytes) }
    }

    #[inline]
    pub fn start_line(&self, id: usize) -> usize {
        self.records[id].start_line as usize
    }

    #[inline]
    pub fn end_line(&self, id: usize) -> usize {
        self.records[id].end_line as usize
    }

    #[inline]
    pub fn language(&self, id: usize) -> Option<&str> {
        let idx = self.records[id].lang_idx;
        if idx == LANG_NONE {
            None
        } else {
            Some(&self.languages[idx as usize])
        }
    }

    pub fn view(&self, id: usize) -> ChunkView<'_> {
        let r = &self.records[id];
        ChunkView {
            content: self.content(id),
            file_path: self.file_path(id),
            start_line: r.start_line as usize,
            end_line: r.end_line as usize,
            language: self.language(id),
        }
    }

    pub fn iter(&self) -> ChunkIter<'_> {
        ChunkIter {
            store: self,
            next: 0,
        }
    }

    pub fn to_indexed(&self, id: usize) -> IndexedChunk {
        IndexedChunk {
            content: self.content(id).to_owned(),
            file_path: self.file_path(id).to_owned(),
            start_line: self.start_line(id),
            end_line: self.end_line(id),
            language: self.language(id).map(ToOwned::to_owned),
        }
    }

    /// Pack the store into a single byte blob. See the module docstring for
    /// why this is hand-rolled rather than serde-derived.
    pub fn encode_to_bytes(&self) -> Vec<u8> {
        let records_bytes = self.records.len() * CHUNK_RECORD_BYTES;
        let paths_bytes: usize = self.paths.iter().map(|p| 4 + p.len()).sum();
        let langs_bytes: usize = self.languages.iter().map(|l| 4 + l.len()).sum();
        // header: 4 (tag) + 4 (records.len) + 4 (paths.len) + 4 (langs.len)
        //         + records_bytes + paths_bytes + langs_bytes
        //         + 4 (content_bytes.len) + content_bytes
        let cap = 4 + 4 * 3 + records_bytes + paths_bytes + langs_bytes + 4 + self.content_bytes.len();
        let mut out = Vec::with_capacity(cap);
        out.extend_from_slice(&STORE_FORMAT_TAG.to_le_bytes());
        out.extend_from_slice(&(self.records.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.paths.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.languages.len() as u32).to_le_bytes());
        // Safety: `ChunkRecord` is `#[repr(C)]` with six packed integers; the
        // platforms we run on share LE byte order, so a raw slice cast gives
        // the same bytes the field-by-field path would.
        let record_slice: &[u8] = unsafe {
            std::slice::from_raw_parts(self.records.as_ptr() as *const u8, records_bytes)
        };
        out.extend_from_slice(record_slice);
        for path in &self.paths {
            out.extend_from_slice(&(path.len() as u32).to_le_bytes());
            out.extend_from_slice(path.as_bytes());
        }
        for lang in &self.languages {
            out.extend_from_slice(&(lang.len() as u32).to_le_bytes());
            out.extend_from_slice(lang.as_bytes());
        }
        out.extend_from_slice(&(self.content_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.content_bytes);
        out
    }

    /// Decode a byte blob produced by `encode_to_bytes`. Errors map to
    /// strings so they thread cleanly into `AppError::internal` at the
    /// caller without dragging the `AppError` enum down into this module.
    pub fn decode_from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let mut cursor = ByteReader::new(bytes);
        let tag = cursor.read_u32()?;
        if tag != STORE_FORMAT_TAG {
            return Err(format!(
                "chunk store tag mismatch: got {tag:#x}, expected {STORE_FORMAT_TAG:#x}",
            ));
        }
        let record_count = cursor.read_u32()? as usize;
        let path_count = cursor.read_u32()? as usize;
        let lang_count = cursor.read_u32()? as usize;

        let record_bytes_total = record_count
            .checked_mul(CHUNK_RECORD_BYTES)
            .ok_or_else(|| "chunk record count overflow".to_string())?;
        let record_slice = cursor.read_slice(record_bytes_total)?;
        // Safety: the slice length is `record_count * size_of::<ChunkRecord>()`
        // and `ChunkRecord` is `#[repr(C)]` over six `u32`/`i32` words, which
        // are 4-byte aligned. SQLite blobs are 8-byte aligned on platforms
        // we ship; copy via `to_vec` rather than pointer-cast to keep the
        // safety story aligned-independent.
        let mut records: Vec<ChunkRecord> = Vec::with_capacity(record_count);
        unsafe {
            records.set_len(record_count);
            let dst =
                std::slice::from_raw_parts_mut(records.as_mut_ptr() as *mut u8, record_bytes_total);
            dst.copy_from_slice(record_slice);
        }

        let mut paths: Vec<Box<str>> = Vec::with_capacity(path_count);
        for _ in 0..path_count {
            let len = cursor.read_u32()? as usize;
            let slice = cursor.read_slice(len)?;
            let s = std::str::from_utf8(slice).map_err(|e| format!("path utf8: {e}"))?;
            paths.push(s.to_owned().into_boxed_str());
        }

        let mut languages: Vec<Box<str>> = Vec::with_capacity(lang_count);
        for _ in 0..lang_count {
            let len = cursor.read_u32()? as usize;
            let slice = cursor.read_slice(len)?;
            let s = std::str::from_utf8(slice).map_err(|e| format!("language utf8: {e}"))?;
            languages.push(s.to_owned().into_boxed_str());
        }

        let content_len = cursor.read_u32()? as usize;
        let content_slice = cursor.read_slice(content_len)?;
        let content_bytes = content_slice.to_vec();

        if cursor.remaining() != 0 {
            return Err(format!(
                "chunk store trailing bytes: {} unread",
                cursor.remaining()
            ));
        }
        Ok(Self {
            records,
            paths,
            languages,
            content_bytes,
        })
    }
}

/// Hand-rolled reader; reaching for `byteorder` for three integer types
/// felt like extra surface area for no real win.
struct ByteReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn read_u32(&mut self) -> Result<u32, String> {
        let slice = self.read_slice(4)?;
        let mut buf = [0u8; 4];
        buf.copy_from_slice(slice);
        Ok(u32::from_le_bytes(buf))
    }

    fn read_slice(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| "chunk store length overflow".to_string())?;
        if end > self.bytes.len() {
            return Err(format!(
                "chunk store truncated: need {end} bytes, have {}",
                self.bytes.len()
            ));
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }
}


/// Borrowed view of one chunk; no allocations. Used by hot paths in
/// ranking that previously did `&chunks[id]` against `Vec<IndexedChunk>`.
#[derive(Clone, Copy, Debug)]
pub struct ChunkView<'a> {
    pub content: &'a str,
    pub file_path: &'a str,
    pub start_line: usize,
    pub end_line: usize,
    pub language: Option<&'a str>,
}

pub struct ChunkIter<'a> {
    store: &'a ChunkStore,
    next: usize,
}

impl<'a> Iterator for ChunkIter<'a> {
    type Item = ChunkView<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= self.store.len() {
            return None;
        }
        let view = self.store.view(self.next);
        self.next += 1;
        Some(view)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.store.len().saturating_sub(self.next);
        (remaining, Some(remaining))
    }
}

impl<'a> ExactSizeIterator for ChunkIter<'a> {}

#[cfg(test)]
mod tests {
    use super::ChunkStore;
    use crate::search::types::IndexedChunk;

    fn sample() -> Vec<IndexedChunk> {
        vec![
            IndexedChunk {
                content: "fn one() {}".to_owned(),
                file_path: "src/a.rs".to_owned(),
                start_line: 1,
                end_line: 1,
                language: Some("rust".to_owned()),
            },
            IndexedChunk {
                content: "fn two() {}".to_owned(),
                file_path: "src/a.rs".to_owned(),
                start_line: 3,
                end_line: 3,
                language: Some("rust".to_owned()),
            },
            IndexedChunk {
                content: "fn three() {}".to_owned(),
                file_path: "README".to_owned(),
                start_line: 1,
                end_line: 1,
                language: None,
            },
        ]
    }

    #[test]
    fn round_trip_preserves_fields() {
        let store = ChunkStore::from_indexed(sample());
        assert_eq!(store.len(), 3);
        assert_eq!(store.content(0), "fn one() {}");
        assert_eq!(store.content(2), "fn three() {}");
        assert_eq!(store.file_path(0), "src/a.rs");
        assert_eq!(store.file_path(1), "src/a.rs");
        assert_eq!(store.file_path(2), "README");
        assert_eq!(store.language(0), Some("rust"));
        assert_eq!(store.language(2), None);
    }

    #[test]
    fn interns_repeated_paths() {
        let store = ChunkStore::from_indexed(sample());
        // The shared file path between chunk 0 and 1 should live at the same
        // path-table slot.
        let r0 = &store.records[0];
        let r1 = &store.records[1];
        let r2 = &store.records[2];
        assert_eq!(r0.path_idx, r1.path_idx);
        assert_ne!(r0.path_idx, r2.path_idx);
    }

    #[test]
    fn to_indexed_recovers_original_shape() {
        let original = sample();
        let store = ChunkStore::from_indexed(original.clone());
        let recovered: Vec<_> = (0..store.len()).map(|i| store.to_indexed(i)).collect();
        assert_eq!(recovered, original);
    }
}
