use std::path::Path;

use crate::error::AppError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
        }
    }

    pub fn is_parseable(self) -> bool {
        matches!(
            self,
            Self::Rust
                | Self::TypeScript
                | Self::Tsx
                | Self::Python
                | Self::Kotlin
                | Self::Prisma
        )
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::Language;

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
}
