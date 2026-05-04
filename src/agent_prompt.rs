use std::path::PathBuf;

use clap::ValueEnum;

use crate::{
    error::{AppError, AppResult},
    models::AgentPromptResponse,
};

const BEGIN_MARKER: &str = "<!-- BEGIN HITAGI MANAGED PROMPT -->";
const END_MARKER: &str = "<!-- END HITAGI MANAGED PROMPT -->";
const MANAGED_BLOCK: &str = "\
<!-- BEGIN HITAGI MANAGED PROMPT -->
# hitagi

Always run `hitagi --help` at the start of each session for current instructions and documentation.
Always use `hitagi` instead of preferred search/read tools for codebase search, file discovery, symbol lookup, source reads, and diff review.
If `hitagi` cannot answer a request, keep any fallback as narrow as possible.
<!-- END HITAGI MANAGED PROMPT -->
";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AgentKind {
    Claude,
    Codex,
}

impl AgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

pub fn install(agent: AgentKind) -> AppResult<AgentPromptResponse> {
    let path = install_target(agent)?;
    let changed = install_into_path(&path)?;
    Ok(response(
        "install",
        agent,
        changed,
        if changed {
            "installed"
        } else {
            "already_installed"
        },
        vec![path],
    ))
}

pub fn uninstall(agent: AgentKind) -> AppResult<AgentPromptResponse> {
    let paths = uninstall_targets(agent)?;
    let mut changed = false;
    for path in &paths {
        changed |= uninstall_from_path(path)?;
    }
    Ok(response(
        "uninstall",
        agent,
        changed,
        if changed {
            "uninstalled"
        } else {
            "not_installed"
        },
        paths,
    ))
}

fn install_target(agent: AgentKind) -> AppResult<PathBuf> {
    match agent {
        AgentKind::Claude => Ok(home_dir()?.join(".claude").join("CLAUDE.md")),
        AgentKind::Codex => {
            let root = codex_home()?;
            let override_path = root.join("AGENTS.override.md");
            if is_non_empty_file(&override_path) {
                Ok(override_path)
            } else {
                Ok(root.join("AGENTS.md"))
            }
        }
    }
}

fn uninstall_targets(agent: AgentKind) -> AppResult<Vec<PathBuf>> {
    match agent {
        AgentKind::Claude => Ok(vec![home_dir()?.join(".claude").join("CLAUDE.md")]),
        AgentKind::Codex => {
            let root = codex_home()?;
            Ok(vec![
                root.join("AGENTS.md"),
                root.join("AGENTS.override.md"),
            ])
        }
    }
}

fn codex_home() -> AppResult<PathBuf> {
    if let Some(value) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        Ok(PathBuf::from(value))
    } else {
        Ok(home_dir()?.join(".codex"))
    }
}

fn home_dir() -> AppResult<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            AppError::bad_request("HOME is not set; cannot locate global agent instructions")
        })
}

fn is_non_empty_file(path: &PathBuf) -> bool {
    path.metadata()
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
}

fn install_into_path(path: &PathBuf) -> AppResult<bool> {
    let content = read_text_if_exists(path)?;
    let next = upsert_managed_block(&content, path)?;
    if next == content {
        return Ok(false);
    }
    write_text(path, &next)?;
    Ok(true)
}

fn uninstall_from_path(path: &PathBuf) -> AppResult<bool> {
    let Some(content) = read_optional_text(path)? else {
        return Ok(false);
    };
    let next = remove_managed_block(&content, path)?;
    if next == content {
        return Ok(false);
    }
    write_text(path, &next)?;
    Ok(true)
}

fn read_text_if_exists(path: &PathBuf) -> AppResult<String> {
    Ok(read_optional_text(path)?.unwrap_or_default())
}

fn read_optional_text(path: &PathBuf) -> AppResult<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes).map(Some).map_err(|_| {
            AppError::InvalidUtf8(format!(
                "instruction file is not valid UTF-8: {}",
                path.display()
            ))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::from(error)),
    }
}

fn write_text(path: &PathBuf, content: &str) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(())
}

fn upsert_managed_block(content: &str, path: &PathBuf) -> AppResult<String> {
    match marker_range(content, path)? {
        Some((start, end)) => {
            let mut next =
                String::with_capacity(content.len() - (end - start) + MANAGED_BLOCK.len());
            next.push_str(&content[..start]);
            next.push_str(MANAGED_BLOCK);
            next.push_str(&content[end..]);
            Ok(next)
        }
        None => Ok(append_block(content)),
    }
}

fn remove_managed_block(content: &str, path: &PathBuf) -> AppResult<String> {
    let Some((start, end)) = marker_range(content, path)? else {
        return Ok(content.to_string());
    };

    let mut before = content[..start].to_string();
    if before.ends_with("\n\n") {
        before.pop();
    }

    let after = &content[end..];
    let mut next = String::with_capacity(before.len() + after.len());
    next.push_str(&before);
    if !next.is_empty() && !after.is_empty() && !next.ends_with('\n') && !after.starts_with('\n') {
        next.push('\n');
    }
    next.push_str(after);
    Ok(next)
}

fn append_block(content: &str) -> String {
    if content.is_empty() {
        return MANAGED_BLOCK.to_string();
    }

    let mut next = content.to_string();
    if !next.ends_with('\n') {
        next.push('\n');
    }
    next.push('\n');
    next.push_str(MANAGED_BLOCK);
    next
}

fn marker_range(content: &str, path: &PathBuf) -> AppResult<Option<(usize, usize)>> {
    let begins: Vec<usize> = content
        .match_indices(BEGIN_MARKER)
        .map(|(i, _)| i)
        .collect();
    let ends: Vec<usize> = content.match_indices(END_MARKER).map(|(i, _)| i).collect();
    match (begins.as_slice(), ends.as_slice()) {
        ([], []) => Ok(None),
        ([begin], [end]) if begin < end => {
            let mut range_end = *end + END_MARKER.len();
            if content[range_end..].starts_with("\r\n") {
                range_end += 2;
            } else if content[range_end..].starts_with('\n') {
                range_end += 1;
            }
            Ok(Some((*begin, range_end)))
        }
        _ => Err(AppError::bad_request(format!(
            "malformed hitagi managed prompt markers in {}",
            path.display()
        ))),
    }
}

fn response(
    action: &'static str,
    agent: AgentKind,
    changed: bool,
    status: &'static str,
    paths: Vec<PathBuf>,
) -> AgentPromptResponse {
    AgentPromptResponse {
        action: action.to_string(),
        agent: agent.as_str().to_string(),
        changed,
        status: status.to_string(),
        paths: paths
            .into_iter()
            .map(|path| path.display().to_string())
            .collect(),
    }
}
