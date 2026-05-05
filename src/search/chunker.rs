//! Source chunking for the search index.
//!
//! Chunk boundaries come from `tree-sitter-language-pack` only. Unsupported
//! files are still walked and fingerprinted, but they do not produce indexed
//! source chunks.

use crate::{
    error::{AppError, AppResult},
    lang::Language,
};

use super::types::IndexedChunk;

const CHUNK_TARGET_BYTES: usize = 2000;

pub fn chunk_source(
    source: &str,
    file_path: &str,
    language: Option<&Language>,
) -> AppResult<Vec<IndexedChunk>> {
    if source.trim().is_empty() {
        return Ok(Vec::new());
    }

    let Some(language) = language.filter(|lang| lang.is_parseable()) else {
        return Ok(Vec::new());
    };

    let config = tree_sitter_language_pack::ProcessConfig::new(language.as_str())
        .minimal()
        .with_chunking(CHUNK_TARGET_BYTES);
    let processed = tree_sitter_language_pack::process(source, &config).map_err(|error| {
        AppError::parse(format!(
            "failed to chunk {} with tree-sitter-language-pack: {error}",
            language.as_str()
        ))
    })?;

    Ok(processed
        .chunks
        .into_iter()
        .filter(|chunk| !chunk.content.trim().is_empty())
        .map(|chunk| IndexedChunk {
            content: chunk.content,
            file_path: file_path.to_owned(),
            start_line: chunk.start_line + 1,
            end_line: chunk.end_line + 1,
            language: Some(language.as_str().to_string()),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use crate::lang::Language;

    use super::chunk_source;

    #[test]
    fn chunk_source_skips_unknown_language() {
        let source = "alpha\nbeta\ngamma\n".repeat(40);
        let chunks = chunk_source(&source, "data.unknown", None).unwrap();
        assert!(chunks.is_empty());
    }

    #[test]
    fn chunk_source_uses_pack_chunks_for_parseable_language() {
        let source = r#"
fn alpha() -> i32 {
    1
}

fn beta() -> i32 {
    2
}
"#;

        let chunks = chunk_source(source, "src/lib.rs", Some(&Language::new("rust"))).unwrap();
        assert!(!chunks.is_empty());
        assert_eq!(chunks[0].file_path, "src/lib.rs");
        assert_eq!(chunks[0].language.as_deref(), Some("rust"));
        assert!(chunks[0].start_line >= 1);
        assert!(chunks[0].end_line >= chunks[0].start_line);
    }
}
