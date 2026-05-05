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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Language {
    Named(String),
    Plaintext,
}

impl Language {
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into().to_ascii_lowercase();
        if name == "plaintext" {
            Self::Plaintext
        } else {
            Self::Named(name)
        }
    }

    pub fn detect(path: &Path) -> Result<Self, AppError> {
        let filename = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("");

        if filename.eq_ignore_ascii_case("Dockerfile")
            || filename.eq_ignore_ascii_case("Containerfile")
        {
            return Ok(Self::new("dockerfile"));
        }

        if filename.eq_ignore_ascii_case("Makefile") {
            return Ok(Self::new("make"));
        }

        if matches!(
            path.extension().and_then(|value| value.to_str()),
            Some(ext) if ext.eq_ignore_ascii_case("txt")
                || ext.eq_ignore_ascii_case("text")
                || ext.eq_ignore_ascii_case("log")
        ) {
            return Err(AppError::unsupported(format!(
                "unsupported file: {}",
                path.display()
            )));
        }

        let path_str = path.to_string_lossy();
        tree_sitter_language_pack::detect_language_from_path(&path_str)
            .map(Self::new)
            .ok_or_else(|| AppError::unsupported(format!("unsupported file: {}", path.display())))
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Named(name) => name,
            Self::Plaintext => "plaintext",
        }
    }

    pub fn is_parseable(&self) -> bool {
        !matches!(self, Self::Plaintext)
    }

    pub fn comment_rules(&self) -> CommentRules {
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
        match self.as_str() {
            "rust" | "kotlin" | "swift" | "scala" | "dart" => SLASHES_NESTED,
            "typescript" | "tsx" | "prisma" | "go" | "java" | "javascript" | "php" | "c"
            | "cpp" | "csharp" | "zig" => SLASHES,
            "css" | "scss" | "sass" | "less" => CommentRules {
                line: &[],
                block: Some(("/*", "*/")),
                nestable_block: false,
            },
            "python" | "yaml" | "toml" | "bash" | "shell" | "dockerfile" | "ruby" | "elixir"
            | "make" => HASH,
            "sql" => CommentRules {
                line: &["--"],
                block: Some(("/*", "*/")),
                nestable_block: false,
            },
            "html" | "xml" => CommentRules {
                line: &[],
                block: Some(("<!--", "-->")),
                nestable_block: false,
            },
            "lua" => CommentRules {
                line: &["--"],
                block: Some(("--[[", "]]")),
                nestable_block: false,
            },
            "haskell" => CommentRules {
                line: &["--"],
                block: Some(("{-", "-}")),
                nestable_block: true,
            },
            "json" | "markdown" | "plaintext" => CommentRules::EMPTY,
            _ => CommentRules::EMPTY,
        }
    }
}

/// One linear byte scan that classifies each line as blank, comment, or code.
/// String-literal tracking covers `"..."` with backslash escape, plus a tiny
/// peek for `'...'` char literals (so `'"'` doesn't open a string). Raw strings
/// with `#` delimiters and triple-quoted strings are best-effort.
///
/// `total` matches cloc's logical-line semantics: a final non-newline-terminated
/// line counts. Differs from a raw `\n`-count by +1 on files without trailing
/// newline.
pub fn count_lines(bytes: &[u8], language: &Language) -> LineStats {
    if bytes.is_empty() {
        return LineStats::default();
    }

    let rules = language.comment_rules();
    let mut stats = LineStats::default();
    let mut line_start = 0usize;
    let mut i = 0usize;
    let mut in_block = false;
    let mut block_depth = 0usize;
    let mut in_string: Option<u8> = None;
    let mut escaped = false;
    let mut line_has_code = false;
    let mut line_has_comment = false;

    while i < bytes.len() {
        let b = bytes[i];

        if b == b'\n' {
            classify_line(
                bytes,
                line_start,
                i,
                line_has_code,
                line_has_comment,
                &mut stats,
            );
            i += 1;
            line_start = i;
            line_has_code = false;
            line_has_comment = false;
            escaped = false;
            continue;
        }

        if in_block {
            line_has_comment = true;
            if let Some((open, close)) = rules.block {
                let rest = &bytes[i..];
                if rules.nestable_block && rest.starts_with(open.as_bytes()) {
                    block_depth += 1;
                    i += open.len();
                    continue;
                }
                if rest.starts_with(close.as_bytes()) {
                    block_depth = block_depth.saturating_sub(1);
                    i += close.len();
                    if block_depth == 0 {
                        in_block = false;
                    }
                    continue;
                }
            }
            i += 1;
            continue;
        }

        if let Some(quote) = in_string {
            line_has_code = true;
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == quote {
                in_string = None;
            }
            i += 1;
            continue;
        }

        if b == b'"' {
            in_string = Some(b'"');
            line_has_code = true;
            i += 1;
            continue;
        }
        if b == b'\'' {
            if let Some(end) = char_literal_end(bytes, i) {
                line_has_code = true;
                i = end + 1;
                continue;
            }
        }

        let rest = &bytes[i..];
        if let Some((open, _)) = rules.block {
            if rest.starts_with(open.as_bytes()) {
                in_block = true;
                block_depth = 1;
                line_has_comment = true;
                i += open.len();
                continue;
            }
        }
        if rules
            .line
            .iter()
            .any(|marker| rest.starts_with(marker.as_bytes()))
        {
            line_has_comment = true;
            i = bytes[i..]
                .iter()
                .position(|&c| c == b'\n')
                .map(|pos| i + pos)
                .unwrap_or(bytes.len());
            continue;
        }

        if !b.is_ascii_whitespace() {
            line_has_code = true;
        }
        i += 1;
    }

    if line_start < bytes.len() {
        classify_line(
            bytes,
            line_start,
            bytes.len(),
            line_has_code,
            line_has_comment || in_block,
            &mut stats,
        );
    } else if bytes.ends_with(b"\n") {
        // Nothing to add: a trailing newline does not create another logical line.
    }

    stats
}

fn char_literal_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    while i < bytes.len() && bytes[i] != b'\n' {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == b'\'' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn classify_line(
    bytes: &[u8],
    start: usize,
    end: usize,
    has_code: bool,
    has_comment: bool,
    stats: &mut LineStats,
) {
    stats.total += 1;
    if bytes[start..end].iter().all(|b| b.is_ascii_whitespace()) {
        stats.blank += 1;
    } else if has_comment && !has_code {
        stats.comment += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(src: &str, lang: &str) -> LineStats {
        count_lines(src.as_bytes(), &Language::new(lang))
    }

    #[test]
    fn detects_pack_languages() {
        assert_eq!(
            Language::detect(Path::new("src/main.rs")).unwrap().as_str(),
            "rust"
        );
        assert_eq!(
            Language::detect(Path::new("app.test.tsx"))
                .unwrap()
                .as_str(),
            "tsx"
        );
        assert_eq!(
            Language::detect(Path::new("cmd/server.go"))
                .unwrap()
                .as_str(),
            "go"
        );
        assert_eq!(
            Language::detect(Path::new("Dockerfile")).unwrap().as_str(),
            "dockerfile"
        );
    }

    #[test]
    fn parseable_is_pack_driven() {
        assert!(Language::new("rust").is_parseable());
        assert!(Language::new("go").is_parseable());
        assert!(!Language::Plaintext.is_parseable());
    }

    #[test]
    fn unknown_extension_errors() {
        assert!(Language::detect(Path::new("x.nope")).is_err());
    }

    #[test]
    fn text_extensions_stay_plaintext() {
        assert!(Language::detect(Path::new("notes.txt")).is_err());
    }

    #[test]
    fn rust_basic() {
        assert_eq!(stats("fn main() {}\n// hi\n\n", "rust").total, 3);
    }

    #[test]
    fn rust_block_comment_multiline() {
        let s = "/* a\n * b\n */\nfn main() {}\n";
        assert_eq!(stats(s, "rust").comment, 3);
    }

    #[test]
    fn rust_nested_block_comment() {
        let s = "/* a /* b */ c */\nfn main() {}\n";
        assert_eq!(stats(s, "rust").comment, 1);
    }

    #[test]
    fn no_trailing_newline_counts_final_line() {
        assert_eq!(stats("fn main() {}", "rust").total, 1);
    }

    #[test]
    fn empty_file_yields_zeros() {
        assert_eq!(stats("", "rust"), LineStats::default());
    }

    #[test]
    fn python_hash_comments() {
        assert_eq!(stats("# hi\nx = 1\n", "python").comment, 1);
    }

    #[test]
    fn css_block_only() {
        assert_eq!(stats("/* hi */\nbody {}\n", "css").comment, 1);
    }

    #[test]
    fn html_only_block() {
        assert_eq!(stats("<!-- hi -->\n<div></div>\n", "html").comment, 1);
    }

    #[test]
    fn json_treats_all_as_code() {
        let s = "{\n  \"a\": 1\n}\n";
        let got = stats(s, "json");
        assert_eq!(got.comment, 0);
        assert_eq!(got.total, 3);
    }

    #[test]
    fn markdown_treats_all_nonblank_as_code() {
        let s = "# Title\n\ntext\n";
        let got = stats(s, "markdown");
        assert_eq!(got.blank, 1);
        assert_eq!(got.comment, 0);
    }

    #[test]
    fn rust_char_literal_with_quote_does_not_open_string() {
        assert_eq!(stats("let c = '\\''; // quote\n", "rust").comment, 0);
    }

    #[test]
    fn rust_byte_char_literal_with_quote() {
        assert_eq!(stats("let c = b'\\''; // quote\n", "rust").comment, 0);
    }

    #[test]
    fn rust_lifetime_does_not_break_scanner() {
        assert_eq!(stats("let x: &'a str = \"//\";\n", "rust").comment, 0);
    }

    #[test]
    fn slashes_inside_string_are_not_a_comment() {
        assert_eq!(stats("let s = \"// not\";\n", "rust").comment, 0);
    }

    #[test]
    fn block_open_inside_string_is_not_a_comment() {
        assert_eq!(stats("let s = \"/* not */\";\n", "rust").comment, 0);
    }

    #[test]
    fn multiline_string_lines_count_as_code() {
        let got = stats("let s = \"a\nb\";\n", "rust");
        assert_eq!(got.total, 2);
        assert_eq!(got.blank, 0);
    }

    #[test]
    fn escaped_quote_inside_string_does_not_close_it() {
        assert_eq!(stats("let s = \"\\\" // nope\";\n", "rust").comment, 0);
    }

    #[test]
    fn sql_double_dash_line_comment() {
        assert_eq!(stats("-- hi\nselect 1\n", "sql").comment, 1);
    }
}
