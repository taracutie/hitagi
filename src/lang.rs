use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::AppError;

/// Per-file line-count breakdown. `code = total - blank - comment` (derived
/// at render time). `total` already includes a final non-newline-terminated
/// line, matching cloc's logical-line semantics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineStats {
    pub total: u32,
    pub blank: u32,
    pub comment: u32,
}

/// Comment syntax for a language. Languages without comments (Json, Markdown,
/// Plaintext) get `EMPTY` ~ every non-blank line counts as code.
#[derive(Debug, Clone, Copy)]
pub struct CommentRules {
    pub line: &'static [&'static str],
    pub block: Option<(&'static str, &'static str)>,
    pub nestable_block: bool,
}

impl CommentRules {
    const EMPTY: Self = Self {
        line: &[],
        block: None,
        nestable_block: false,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Language {
    // Parseable (have tree-sitter grammars)
    Rust,
    TypeScript,
    Tsx,
    Python,
    Kotlin,
    Prisma,
    // Recognized but no parser ~ outline/symbol/find won't work, but they get a real
    // language label in `langs` and `read` rather than collapsing into "plaintext".
    Json,
    Yaml,
    Toml,
    Markdown,
    Sql,
    Html,
    Css,
    Shell,
    Dockerfile,
    /// Catch-all for unrecognized extensions. `Language::detect` does NOT
    /// return this ~ it still errors so existing callers' `.ok()` paths are
    /// unchanged. Cache callers that need to key plaintext files map their
    /// own Err → Plaintext (see `commands::cache_line_count_for`).
    Plaintext,
}

extern "C" {
    fn tree_sitter_rust() -> tree_sitter::Language;
    fn tree_sitter_typescript() -> tree_sitter::Language;
    fn tree_sitter_tsx() -> tree_sitter::Language;
    fn tree_sitter_python() -> tree_sitter::Language;
    fn tree_sitter_kotlin() -> tree_sitter::Language;
    fn tree_sitter_prisma() -> tree_sitter::Language;
}

impl Language {
    pub fn detect(path: &Path) -> Result<Self, AppError> {
        let filename = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("");

        if filename.eq_ignore_ascii_case("Dockerfile")
            || filename.eq_ignore_ascii_case("Containerfile")
        {
            return Ok(Self::Dockerfile);
        }

        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .ok_or_else(|| {
                AppError::unsupported(format!("unsupported file: {}", path.display()))
            })?;

        let lower = extension.to_ascii_lowercase();
        match lower.as_str() {
            "rs" => Ok(Self::Rust),
            "ts" => Ok(Self::TypeScript),
            "tsx" => Ok(Self::Tsx),
            "py" => Ok(Self::Python),
            "kt" | "kts" => Ok(Self::Kotlin),
            "prisma" => Ok(Self::Prisma),
            "json" | "jsonc" | "json5" => Ok(Self::Json),
            "yaml" | "yml" => Ok(Self::Yaml),
            "toml" => Ok(Self::Toml),
            "md" | "markdown" | "mdx" => Ok(Self::Markdown),
            "sql" => Ok(Self::Sql),
            "html" | "htm" => Ok(Self::Html),
            "css" | "scss" | "sass" | "less" => Ok(Self::Css),
            "sh" | "bash" | "zsh" | "fish" => Ok(Self::Shell),
            _ => Err(AppError::unsupported(format!(
                "unsupported file extension .{extension}"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::Python => "python",
            Self::Kotlin => "kotlin",
            Self::Prisma => "prisma",
            Self::Json => "json",
            Self::Yaml => "yaml",
            Self::Toml => "toml",
            Self::Markdown => "markdown",
            Self::Sql => "sql",
            Self::Html => "html",
            Self::Css => "css",
            Self::Shell => "shell",
            Self::Dockerfile => "dockerfile",
            Self::Plaintext => "plaintext",
        }
    }

    pub fn is_parseable(self) -> bool {
        matches!(
            self,
            Self::Rust | Self::TypeScript | Self::Tsx | Self::Python | Self::Kotlin | Self::Prisma
        )
    }

    pub fn comment_rules(self) -> CommentRules {
        const SLASHES: CommentRules = CommentRules {
            line: &["//"],
            block: Some(("/*", "*/")),
            nestable_block: false,
        };
        const SLASHES_NESTED: CommentRules = CommentRules {
            line: &["//"],
            block: Some(("/*", "*/")),
            nestable_block: true,
        };
        const HASH: CommentRules = CommentRules {
            line: &["#"],
            block: None,
            nestable_block: false,
        };
        match self {
            Self::Rust | Self::Kotlin => SLASHES_NESTED,
            Self::TypeScript | Self::Tsx | Self::Prisma => SLASHES,
            Self::Css => CommentRules {
                line: &[],
                block: Some(("/*", "*/")),
                nestable_block: false,
            },
            Self::Python | Self::Yaml | Self::Toml | Self::Shell | Self::Dockerfile => HASH,
            Self::Sql => CommentRules {
                line: &["--"],
                block: Some(("/*", "*/")),
                nestable_block: false,
            },
            Self::Html => CommentRules {
                line: &[],
                block: Some(("<!--", "-->")),
                nestable_block: false,
            },
            Self::Json | Self::Markdown | Self::Plaintext => CommentRules::EMPTY,
        }
    }

    pub fn tree_sitter_language(self) -> Option<tree_sitter::Language> {
        unsafe {
            match self {
                Self::Rust => Some(tree_sitter_rust()),
                Self::TypeScript => Some(tree_sitter_typescript()),
                Self::Tsx => Some(tree_sitter_tsx()),
                Self::Python => Some(tree_sitter_python()),
                Self::Kotlin => Some(tree_sitter_kotlin()),
                Self::Prisma => Some(tree_sitter_prisma()),
                _ => None,
            }
        }
    }
}

/// One linear byte scan that classifies each line as blank, comment, or code.
/// String-literal tracking covers `"..."` with backslash escape, plus a tiny
/// peek for `'…'` char literals (so `'"'` doesn't open a string). Raw strings
/// with `#` delimiters and triple-quoted strings are best-effort.
///
/// `total` matches cloc's logical-line semantics: a final non-newline-terminated
/// line counts. Differs from a raw `\n`-count by +1 on files without trailing
/// newline.
pub fn count_lines(bytes: &[u8], language: Language) -> LineStats {
    let rules = language.comment_rules();
    let mut stats = LineStats::default();

    let mut block_depth: u32 = 0;
    let mut saw_code = false;
    let mut saw_comment = false;
    let mut line_has_bytes = false;
    // String-literal state survives newlines (multi-line strings count as code).
    let mut in_string = false;
    let mut string_escape = false;
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];

        if b == b'\n' {
            classify_line(&mut stats, saw_code, saw_comment, block_depth > 0);
            saw_code = false;
            saw_comment = false;
            line_has_bytes = false;
            string_escape = false;
            i += 1;
            continue;
        }

        line_has_bytes = true;

        if b == b' ' || b == b'\t' || b == b'\r' {
            i += 1;
            continue;
        }

        if block_depth > 0 {
            saw_comment = true;
            if let Some((open, close)) = rules.block {
                if rules.nestable_block && bytes[i..].starts_with(open.as_bytes()) {
                    block_depth += 1;
                    i += open.len();
                    continue;
                }
                if bytes[i..].starts_with(close.as_bytes()) {
                    block_depth -= 1;
                    i += close.len();
                    continue;
                }
            }
            i += 1;
            continue;
        }

        if in_string {
            saw_code = true;
            if string_escape {
                string_escape = false;
            } else if b == b'\\' {
                string_escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        if let Some((open, close)) = rules.block {
            if bytes[i..].starts_with(open.as_bytes()) {
                block_depth = 1;
                saw_comment = true;
                i += open.len();
                while i < bytes.len() && bytes[i] != b'\n' && block_depth > 0 {
                    if rules.nestable_block && bytes[i..].starts_with(open.as_bytes()) {
                        block_depth += 1;
                        i += open.len();
                    } else if bytes[i..].starts_with(close.as_bytes()) {
                        block_depth -= 1;
                        i += close.len();
                    } else {
                        i += 1;
                    }
                }
                continue;
            }
        }

        if rules.line.iter().any(|m| bytes[i..].starts_with(m.as_bytes())) {
            saw_comment = true;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // Rust char literals like `'"'` / `b'"'` contain a `"` but aren't strings.
        // Lifetimes like `'static` won't find a closing `'` quickly and fall
        // through as ordinary code.
        if b == b'\'' {
            if let Some(end) = char_literal_end(&bytes[i + 1..]) {
                saw_code = true;
                i += 1 + end + 1;
                continue;
            }
            saw_code = true;
            i += 1;
            continue;
        }

        if b == b'"' {
            in_string = true;
            saw_code = true;
            i += 1;
            continue;
        }

        saw_code = true;
        i += 1;
    }

    if line_has_bytes {
        classify_line(&mut stats, saw_code, saw_comment, block_depth > 0);
    }

    stats
}

/// Returns the byte offset of the closing `'` if `body` starts with a plausible
/// char-literal payload (1..=10 bytes, no newline). `None` falls through ~ a
/// stray `'` (e.g. Rust lifetime) is then treated as ordinary code.
fn char_literal_end(body: &[u8]) -> Option<usize> {
    let cap = body.len().min(11);
    let mut k = 0usize;
    while k < cap {
        match body[k] {
            b'\n' => return None,
            b'\\' if k + 1 < cap => k += 2,
            b'\'' if k > 0 => return Some(k),
            _ => k += 1,
        }
    }
    None
}

fn classify_line(stats: &mut LineStats, saw_code: bool, saw_comment: bool, in_block: bool) {
    stats.total = stats.total.saturating_add(1);
    if saw_code {
        // code dominates ~ a line with both code and a trailing comment is code
    } else if saw_comment || in_block {
        stats.comment = stats.comment.saturating_add(1);
    } else {
        stats.blank = stats.blank.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{count_lines, Language, LineStats};

    fn stats(total: u32, blank: u32, comment: u32) -> LineStats {
        LineStats { total, blank, comment }
    }

    #[test]
    fn detects_supported_languages() {
        assert_eq!(
            Language::detect(Path::new("main.rs")).unwrap(),
            Language::Rust
        );
        assert_eq!(
            Language::detect(Path::new("auth.ts")).unwrap(),
            Language::TypeScript
        );
        assert_eq!(
            Language::detect(Path::new("view.tsx")).unwrap(),
            Language::Tsx
        );
        assert_eq!(
            Language::detect(Path::new("tool.py")).unwrap(),
            Language::Python
        );
        assert_eq!(
            Language::detect(Path::new("App.kt")).unwrap(),
            Language::Kotlin
        );
        assert_eq!(
            Language::detect(Path::new("schema.prisma")).unwrap(),
            Language::Prisma
        );
    }

    #[test]
    fn detects_recognized_non_parseable_languages() {
        assert_eq!(
            Language::detect(Path::new("config.json")).unwrap(),
            Language::Json
        );
        assert_eq!(
            Language::detect(Path::new("playbook.yml")).unwrap(),
            Language::Yaml
        );
        assert_eq!(
            Language::detect(Path::new("Cargo.toml")).unwrap(),
            Language::Toml
        );
        assert_eq!(
            Language::detect(Path::new("README.md")).unwrap(),
            Language::Markdown
        );
        assert_eq!(
            Language::detect(Path::new("schema.sql")).unwrap(),
            Language::Sql
        );
        assert_eq!(
            Language::detect(Path::new("page.html")).unwrap(),
            Language::Html
        );
        assert_eq!(
            Language::detect(Path::new("styles.css")).unwrap(),
            Language::Css
        );
        assert_eq!(
            Language::detect(Path::new("install.sh")).unwrap(),
            Language::Shell
        );
        assert_eq!(
            Language::detect(Path::new("Dockerfile")).unwrap(),
            Language::Dockerfile
        );
    }

    #[test]
    fn parseable_distinguishes_with_and_without_grammar() {
        assert!(Language::Rust.is_parseable());
        assert!(Language::TypeScript.is_parseable());
        assert!(!Language::Json.is_parseable());
        assert!(!Language::Markdown.is_parseable());
        assert!(!Language::Dockerfile.is_parseable());
    }

    #[test]
    fn unknown_extension_errors() {
        assert!(Language::detect(Path::new("notes.zzz")).is_err());
    }

    #[test]
    fn rust_basic() {
        let src = b"// header\nfn main() {\n    println!(\"hi\"); // inline\n}\n\n";
        assert_eq!(count_lines(src, Language::Rust), stats(5, 1, 1));
    }

    #[test]
    fn rust_block_comment_multiline() {
        let src = b"/* a\n   b\n   c */\nfn x() {}\n";
        assert_eq!(count_lines(src, Language::Rust), stats(4, 0, 3));
    }

    #[test]
    fn rust_nested_block_comment() {
        let src = b"/* a /* b */ c */ x\n";
        assert_eq!(count_lines(src, Language::Rust), stats(1, 0, 0));
        let src2 = b"/* a /* b */ c */\n";
        assert_eq!(count_lines(src2, Language::Rust), stats(1, 0, 1));
    }

    #[test]
    fn no_trailing_newline_counts_final_line() {
        let src = b"fn x() {}";
        assert_eq!(count_lines(src, Language::Rust), stats(1, 0, 0));
    }

    #[test]
    fn empty_file_yields_zeros() {
        assert_eq!(count_lines(b"", Language::Rust), stats(0, 0, 0));
        assert_eq!(count_lines(b"\n", Language::Rust), stats(1, 1, 0));
    }

    #[test]
    fn python_hash_comments() {
        let src = b"# top\nx = 1\n\n# trailing\n";
        assert_eq!(count_lines(src, Language::Python), stats(4, 1, 2));
    }

    #[test]
    fn css_block_only() {
        let src = b"/* a */\n// not-a-comment\nbody {}\n";
        assert_eq!(count_lines(src, Language::Css), stats(3, 0, 1));
    }

    #[test]
    fn html_only_block() {
        let src = b"<!-- top -->\n<p>hi</p>\n";
        assert_eq!(count_lines(src, Language::Html), stats(2, 0, 1));
    }

    #[test]
    fn json_treats_all_as_code() {
        let src = b"{\n  \"a\": 1\n}\n";
        assert_eq!(count_lines(src, Language::Json), stats(3, 0, 0));
    }

    #[test]
    fn markdown_treats_all_nonblank_as_code() {
        let src = b"# Title\n\nbody\n";
        assert_eq!(count_lines(src, Language::Markdown), stats(3, 1, 0));
    }

    #[test]
    fn rust_char_literal_with_quote_does_not_open_string() {
        let src = b"if c == '\"' {} // tail\n";
        assert_eq!(count_lines(src, Language::Rust), stats(1, 0, 0));
    }

    #[test]
    fn rust_byte_char_literal_with_quote() {
        let src = b"if b == b'\"' {}\n// after\n";
        assert_eq!(count_lines(src, Language::Rust), stats(2, 0, 1));
    }

    #[test]
    fn rust_lifetime_does_not_break_scanner() {
        let src = b"fn x<'a>(s: &'static str) {}\n";
        assert_eq!(count_lines(src, Language::Rust), stats(1, 0, 0));
    }

    #[test]
    fn slashes_inside_string_are_not_a_comment() {
        let src = b"let s = \"http://example.com\";\n";
        assert_eq!(count_lines(src, Language::Rust), stats(1, 0, 0));
    }

    #[test]
    fn block_open_inside_string_is_not_a_comment() {
        let src = b"let s = \"a /* not a comment */ b\";\n";
        assert_eq!(count_lines(src, Language::Rust), stats(1, 0, 0));
    }

    #[test]
    fn multiline_string_lines_count_as_code() {
        let src = b"let s = \"first\nsecond\nthird\";\n";
        assert_eq!(count_lines(src, Language::Rust), stats(3, 0, 0));
    }

    #[test]
    fn escaped_quote_inside_string_does_not_close_it() {
        let src = b"let s = \"he said \\\"//hi\\\"\";\n";
        assert_eq!(count_lines(src, Language::Rust), stats(1, 0, 0));
    }

    #[test]
    fn sql_double_dash_line_comment() {
        let src = b"-- top\nSELECT 1;\n/* block */\n";
        assert_eq!(count_lines(src, Language::Sql), stats(3, 0, 2));
    }
}
