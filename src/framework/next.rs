//! Next.js framework queries: project detection, route enumeration, layout
//! discovery, server-action listing.
//!
//! Detection reads `<root>/package.json`. Routes/layouts walk `app/` and
//! `pages/` (with `src/` variants), driven by Next.js's filesystem
//! conventions. HTTP methods on app-router API routes and server-action
//! discovery use tree-sitter (via `parser::parse_source`).

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use ignore::WalkBuilder;
use once_cell::sync::Lazy;
use regex::Regex;

use crate::error::{AppError, AppResult};
use crate::lang::Language;
use crate::models::{
    NextInfoResponse, NextLayout, NextLayoutsResponse, NextRoute, NextRoutesResponse,
    NextServerAction, NextServerActionsResponse, SymbolInfo,
};
use crate::parser::parse_source;
use crate::repo::RepoRoot;

const ROUTE_EXTS: &[&str] = &["tsx", "ts", "jsx", "js"];
const HTTP_VERBS: &[&str] = &["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"];
const LAYOUT_KINDS: &[(&str, &str)] = &[
    ("layout", "layout"),
    ("template", "template"),
    ("loading", "loading"),
    ("error", "error"),
    ("not-found", "not-found"),
    ("default", "default"),
    ("global-error", "global-error"),
];

struct NextProject {
    version: String,
    app_dir: Option<PathBuf>,
    pages_dir: Option<PathBuf>,
    src_layout: bool,
    project_root: PathBuf,
    repo_root: PathBuf,
    root_relative: String,
}

pub fn next_info(repo: &RepoRoot, root: Option<&str>) -> AppResult<NextInfoResponse> {
    let project = detect(repo, root)?;
    let router = match (&project.app_dir, &project.pages_dir) {
        (Some(_), Some(_)) => Some("both".to_string()),
        (Some(_), None) => Some("app".to_string()),
        (None, Some(_)) => Some("pages".to_string()),
        (None, None) => None,
    };
    Ok(NextInfoResponse {
        framework: "next",
        detected: true,
        version: Some(project.version),
        router,
        src_layout: project.src_layout,
        root: project.root_relative,
    })
}

pub fn next_list_routes(repo: &RepoRoot, root: Option<&str>) -> AppResult<NextRoutesResponse> {
    let project = detect(repo, root)?;
    let mut routes = Vec::new();

    if let Some(app_dir) = &project.app_dir {
        collect_app_routes(app_dir, &project.repo_root, &mut routes);
    }
    if let Some(pages_dir) = &project.pages_dir {
        collect_pages_routes(pages_dir, &project.repo_root, &mut routes);
    }

    routes.sort_by(|a, b| a.pattern.cmp(&b.pattern).then_with(|| a.file.cmp(&b.file)));

    Ok(NextRoutesResponse {
        framework: "next",
        root: project.root_relative,
        routes,
    })
}

pub fn next_list_layouts(repo: &RepoRoot, root: Option<&str>) -> AppResult<NextLayoutsResponse> {
    let project = detect(repo, root)?;
    let mut layouts = Vec::new();

    if let Some(app_dir) = &project.app_dir {
        collect_layouts(app_dir, &project.repo_root, &mut layouts);
    }

    layouts.sort_by(|a, b| {
        a.scope
            .cmp(&b.scope)
            .then_with(|| a.kind.cmp(b.kind))
            .then_with(|| a.file.cmp(&b.file))
    });

    Ok(NextLayoutsResponse {
        framework: "next",
        root: project.root_relative,
        layouts,
    })
}

pub fn next_list_server_actions(
    repo: &RepoRoot,
    root: Option<&str>,
) -> AppResult<NextServerActionsResponse> {
    let project = detect(repo, root)?;
    let mut actions = Vec::new();
    let mut walked: HashSet<PathBuf> = HashSet::new();

    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(p) = &project.app_dir {
        roots.push(p.clone());
    }
    if let Some(p) = &project.pages_dir {
        roots.push(p.clone());
    }
    if roots.is_empty() {
        roots.push(project.project_root.clone());
    }

    for start in &roots {
        for full_path in walk_files(start) {
            if !walked.insert(full_path.clone()) {
                continue;
            }
            collect_server_actions_from(&full_path, &project.repo_root, &mut actions);
        }
    }

    actions.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.name.cmp(&b.name)));

    Ok(NextServerActionsResponse {
        framework: "next",
        root: project.root_relative,
        actions,
    })
}

// ~~ detection ~~

fn detect(repo: &RepoRoot, root: Option<&str>) -> AppResult<NextProject> {
    let (root_path, root_relative) = resolve_root(repo, root)?;
    let pkg_path = root_path.join("package.json");
    let pkg_text = std::fs::read_to_string(&pkg_path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            AppError::not_found(format!(
                "framework not detected: package.json not found at {}/package.json",
                root_relative
            ))
        } else {
            AppError::from(err)
        }
    })?;
    let pkg: serde_json::Value = serde_json::from_str(&pkg_text).map_err(|err| {
        AppError::parse(format!(
            "framework: failed to parse {}/package.json: {err}",
            root_relative
        ))
    })?;

    let version = find_next_dep(&pkg).ok_or_else(|| {
        AppError::not_found(format!(
            "framework not detected: next.js not found in {}/package.json (checked dependencies, devDependencies, peerDependencies)",
            root_relative
        ))
    })?;

    let mut app_dir = None;
    let mut pages_dir = None;
    let mut src_layout = false;

    if root_path.join("app").is_dir() {
        app_dir = Some(root_path.join("app"));
    } else if root_path.join("src").join("app").is_dir() {
        app_dir = Some(root_path.join("src").join("app"));
        src_layout = true;
    }

    if root_path.join("pages").is_dir() {
        pages_dir = Some(root_path.join("pages"));
    } else if root_path.join("src").join("pages").is_dir() {
        pages_dir = Some(root_path.join("src").join("pages"));
        src_layout = true;
    }

    Ok(NextProject {
        version,
        app_dir,
        pages_dir,
        src_layout,
        project_root: root_path,
        repo_root: repo.root().to_path_buf(),
        root_relative,
    })
}

fn resolve_root(repo: &RepoRoot, root: Option<&str>) -> AppResult<(PathBuf, String)> {
    match root {
        None => Ok((repo.root().to_path_buf(), ".".to_string())),
        Some(path) if is_explicit_repo_root(path) => {
            Ok((repo.root().to_path_buf(), ".".to_string()))
        }
        Some(path) => {
            let resolved = repo.resolve_file_or_dir(path)?;
            Ok((resolved.full_path, resolved.relative_path))
        }
    }
}

fn is_explicit_repo_root(path: &str) -> bool {
    let mut saw_component = false;
    for component in Path::new(path).components() {
        saw_component = true;
        if !matches!(component, Component::CurDir) {
            return false;
        }
    }
    saw_component
}

fn find_next_dep(pkg: &serde_json::Value) -> Option<String> {
    for key in ["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(deps) = pkg.get(key).and_then(|v| v.as_object()) {
            if let Some(version) = deps.get("next").and_then(|v| v.as_str()) {
                return Some(version.to_string());
            }
        }
    }
    None
}

// ~~ app-router routes ~~

fn collect_app_routes(app_dir: &Path, repo_root: &Path, routes: &mut Vec<NextRoute>) {
    for full_path in walk_files(app_dir) {
        let Some(rel_to_app) = full_path.strip_prefix(app_dir).ok() else {
            continue;
        };
        if has_private_segment(rel_to_app) {
            continue;
        }
        let stem = full_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if !ext_is_route_source(&full_path) {
            continue;
        }

        let kind = match stem {
            "page" => "page",
            "route" => "api",
            _ => continue,
        };

        let dir_segments = path_normal_segments(rel_to_app.parent().unwrap_or(Path::new("")));
        let (pattern, advanced) = compute_app_pattern(&dir_segments);
        let file = relative_to_repo(&full_path, repo_root);

        let methods = if kind == "api" {
            extract_methods(&full_path)
        } else {
            None
        };

        routes.push(NextRoute {
            pattern,
            file,
            kind,
            router: "app",
            methods,
            advanced,
        });
    }
}

fn compute_app_pattern(segments: &[String]) -> (String, bool) {
    let mut pieces: Vec<String> = Vec::new();
    let mut advanced = false;
    for seg in segments {
        if seg.starts_with('@') {
            // Parallel-route slot: doesn't appear in the URL, but signal advanced.
            advanced = true;
            continue;
        }
        if let Some(stripped) = strip_intercepting_prefix(seg) {
            advanced = true;
            if !stripped.is_empty() {
                pieces.push(stripped.to_string());
            }
            continue;
        }
        if seg.starts_with('(') && seg.ends_with(')') {
            // Pure route group ~ dropped.
            continue;
        }
        pieces.push(seg.clone());
    }
    let pattern = if pieces.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", pieces.join("/"))
    };
    (pattern, advanced)
}

fn strip_intercepting_prefix(seg: &str) -> Option<&str> {
    let mut rest = seg;
    let mut stripped = false;
    loop {
        let mut next = None;
        for prefix in &["(...)", "(..)", "(.)"] {
            if let Some(after) = rest.strip_prefix(prefix) {
                next = Some(after);
                break;
            }
        }
        let Some(after) = next else {
            break;
        };
        rest = after;
        stripped = true;
    }
    stripped.then_some(rest)
}

// ~~ pages-router routes ~~

fn collect_pages_routes(pages_dir: &Path, repo_root: &Path, routes: &mut Vec<NextRoute>) {
    for full_path in walk_files(pages_dir) {
        let Some(rel_to_pages) = full_path.strip_prefix(pages_dir).ok() else {
            continue;
        };
        if !ext_is_route_source(&full_path) {
            continue;
        }
        let stem = full_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if stem.starts_with('_') {
            continue;
        }

        let mut segs = path_normal_segments(rel_to_pages);
        if let Some(last) = segs.last_mut() {
            if let Some(idx) = last.rfind('.') {
                last.truncate(idx);
            }
        }

        let is_api = segs.first().map(|s| s.as_str()) == Some("api");
        let kind = if is_api { "api" } else { "page" };

        if segs.last().map_or(false, |s| s == "index") {
            segs.pop();
        }

        // Top-level catch-all special files like "404"/"500" are not routes.
        if segs.len() == 1 && (segs[0] == "404" || segs[0] == "500") {
            continue;
        }

        let pattern = if segs.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", segs.join("/"))
        };
        let file = relative_to_repo(&full_path, repo_root);
        routes.push(NextRoute {
            pattern,
            file,
            kind,
            router: "pages",
            methods: None,
            advanced: false,
        });
    }
}

// ~~ layouts (app router only) ~~

fn collect_layouts(app_dir: &Path, repo_root: &Path, layouts: &mut Vec<NextLayout>) {
    for full_path in walk_files(app_dir) {
        let Some(rel_to_app) = full_path.strip_prefix(app_dir).ok() else {
            continue;
        };
        if has_private_segment(rel_to_app) {
            continue;
        }
        if !ext_is_route_source(&full_path) {
            continue;
        }
        let stem = full_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let Some((_, kind)) = LAYOUT_KINDS.iter().find(|(name, _)| *name == stem) else {
            continue;
        };

        let dir_segments = path_normal_segments(rel_to_app.parent().unwrap_or(Path::new("")));
        let (scope, _) = compute_app_pattern(&dir_segments);
        let file = relative_to_repo(&full_path, repo_root);
        layouts.push(NextLayout { kind, file, scope });
    }
}

// ~~ server actions ~~

fn collect_server_actions_from(
    full_path: &Path,
    repo_root: &Path,
    actions: &mut Vec<NextServerAction>,
) {
    if !ext_is_route_source(full_path) {
        return;
    }
    let Ok(source) = std::fs::read_to_string(full_path) else {
        return;
    };
    if !source.contains("\"use server\"") && !source.contains("'use server'") {
        return;
    }
    let Ok(language) = Language::detect(full_path) else {
        return;
    };
    if !language.is_parseable() {
        return;
    }
    let Ok(parsed) = parse_source(&language, &source) else {
        return;
    };

    let file = relative_to_repo(full_path, repo_root);
    let file_level = file_has_use_server_directive(&source);

    if file_level {
        let names = file_level_server_action_names(&source, &parsed.symbols);
        if names.is_empty() {
            // Directive present but no exported function symbols recognised; record
            // the file with a wildcard name so the caller still sees the signal.
            actions.push(NextServerAction {
                file,
                name: "*".to_string(),
                scope: "file",
            });
        } else {
            for name in names {
                actions.push(NextServerAction {
                    file: file.clone(),
                    name,
                    scope: "file",
                });
            }
        }
    } else {
        let mut seen = HashSet::new();
        for sym in &parsed.symbols {
            if !is_function_like(sym) {
                continue;
            }
            if function_body_starts_with_use_server(&source, sym) && seen.insert(sym.name.clone()) {
                actions.push(NextServerAction {
                    file: file.clone(),
                    name: sym.name.clone(),
                    scope: "function",
                });
            }
        }
        for name in function_level_binding_server_action_names(&source, &language) {
            if seen.insert(name.clone()) {
                actions.push(NextServerAction {
                    file: file.clone(),
                    name,
                    scope: "function",
                });
            }
        }
    }
}

fn file_level_server_action_names(source: &str, symbols: &[SymbolInfo]) -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    let mut local_action_names = HashSet::new();

    for sym in symbols {
        if !is_top_level_function(sym) {
            continue;
        }
        local_action_names.insert(sym.name.clone());
        if !symbol_is_exported(source, sym) {
            continue;
        }
        if seen.insert(sym.name.clone()) {
            names.push(sym.name.clone());
        }
    }

    for cap in EXPORT_ACTION_BINDING_RE.captures_iter(source) {
        let Some(name) = cap.get(1).map(|m| m.as_str().to_string()) else {
            continue;
        };
        if seen.insert(name.clone()) {
            names.push(name);
        }
    }

    for cap in LOCAL_ACTION_BINDING_RE.captures_iter(source) {
        if let Some(name) = cap.get(1).map(|m| m.as_str().to_string()) {
            local_action_names.insert(name);
        }
    }

    for export in export_list_entries(source) {
        if !local_action_names.contains(export.local.as_str()) {
            continue;
        }
        if seen.insert(export.exported.clone()) {
            names.push(export.exported);
        }
    }

    for cap in EXPORT_DEFAULT_IDENTIFIER_RE.captures_iter(source) {
        let Some(local) = cap.get(1).map(|m| m.as_str()) else {
            continue;
        };
        if local_action_names.contains(local) && seen.insert("default".to_string()) {
            names.push("default".to_string());
        }
    }

    if EXPORT_DEFAULT_ACTION_RE.is_match(source) && seen.insert("default".to_string()) {
        names.push("default".to_string());
    }

    names
}

#[derive(Debug, Eq, PartialEq)]
struct ExportListEntry {
    local: String,
    exported: String,
}

static EXPORT_ACTION_BINDING_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?m)^\s*export\s+(?:const|let|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)\b[^;=]*=\s*async\b",
    )
    .expect("EXPORT_ACTION_BINDING_RE compiles")
});

static LOCAL_ACTION_BINDING_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?m)^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)\b[^;=]*=\s*async\b",
    )
    .expect("LOCAL_ACTION_BINDING_RE compiles")
});

static EXPORT_DEFAULT_ACTION_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?m)^\s*export\s+default\s+(?:(?:async\s+)?function\b|async\s*(?:\(|[A-Za-z_$]))")
        .expect("EXPORT_DEFAULT_ACTION_RE compiles")
});

static EXPORT_DEFAULT_IDENTIFIER_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?m)^\s*export\s+default\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*;?\s*(?://.*)?$")
        .expect("EXPORT_DEFAULT_IDENTIFIER_RE compiles")
});

static EXPORT_LIST_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?ms)^\s*export\s+(type\s+)?\{(?P<body>.*?)\}\s*(?:from\s+[^;]+)?;?")
        .expect("EXPORT_LIST_RE compiles")
});

fn export_list_entries(source: &str) -> Vec<ExportListEntry> {
    let mut entries = Vec::new();
    for cap in EXPORT_LIST_RE.captures_iter(source) {
        if cap.get(1).is_some() {
            continue;
        }
        let Some(body) = cap.name("body").map(|m| m.as_str()) else {
            continue;
        };
        for raw in body.split(',') {
            if let Some(entry) = parse_export_list_specifier(raw) {
                entries.push(entry);
            }
        }
    }
    entries
}

fn parse_export_list_specifier(raw: &str) -> Option<ExportListEntry> {
    let spec = raw.trim();
    if spec.is_empty() {
        return None;
    }

    let parts: Vec<&str> = spec.split_whitespace().collect();
    let (local, exported) = match parts.as_slice() {
        [local] => (*local, *local),
        ["type", _] | ["type", _, "as", _] => return None,
        [local, "as", exported] => (*local, *exported),
        _ => return None,
    };

    if !is_js_identifier(local) || !is_js_identifier(exported) {
        return None;
    }

    Some(ExportListEntry {
        local: local.to_string(),
        exported: exported.to_string(),
    })
}

fn is_js_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !matches!(first, b'A'..=b'Z' | b'a'..=b'z' | b'_' | b'$') {
        return false;
    }
    bytes.all(|b| matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'$'))
}

fn function_level_binding_server_action_names(source: &str, language: &Language) -> Vec<String> {
    if !matches!(
        language.as_str(),
        "javascript" | "jsx" | "typescript" | "tsx"
    ) {
        return Vec::new();
    }

    let Ok(mut parser) = tree_sitter_language_pack::get_parser(language.as_str()) else {
        return Vec::new();
    };
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };

    let mut names = Vec::new();
    let mut seen = HashSet::new();
    collect_function_level_binding_server_actions(tree.root_node(), source, &mut names, &mut seen);
    names
}

fn collect_function_level_binding_server_actions(
    node: tree_sitter::Node<'_>,
    source: &str,
    names: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    if node.kind() == "variable_declarator" {
        if let Some(name) = function_level_binding_server_action_name(node, source) {
            if seen.insert(name.clone()) {
                names.push(name);
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_function_level_binding_server_actions(child, source, names, seen);
    }
}

fn function_level_binding_server_action_name(
    node: tree_sitter::Node<'_>,
    source: &str,
) -> Option<String> {
    let name_node = node.child_by_field_name("name")?;
    if name_node.kind() != "identifier" {
        return None;
    }
    let name = name_node.utf8_text(source.as_bytes()).ok()?;
    if !is_js_identifier(name) {
        return None;
    }

    let value_node = node.child_by_field_name("value")?;
    if !matches!(value_node.kind(), "arrow_function" | "function_expression") {
        return None;
    }
    if !function_node_is_async(value_node) {
        return None;
    }
    if !function_node_body_starts_with_use_server(source, value_node) {
        return None;
    }

    Some(name.to_string())
}

fn function_node_is_async(node: tree_sitter::Node<'_>) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "async" && child.start_byte() == node.start_byte() {
            return true;
        }
    }
    false
}

fn function_node_body_starts_with_use_server(source: &str, node: tree_sitter::Node<'_>) -> bool {
    let Some(body) = node.child_by_field_name("body") else {
        return false;
    };
    if body.kind() != "statement_block" {
        return false;
    }

    let end = body.end_byte().min(source.len());
    let start = body.start_byte().min(end);
    let span = &source[start..end];
    let Some(brace_idx) = span.find('{') else {
        return false;
    };
    let after = &span[brace_idx + 1..];
    let trimmed = strip_leading_trivia(after);
    trimmed.starts_with("\"use server\"") || trimmed.starts_with("'use server'")
}

fn file_has_use_server_directive(source: &str) -> bool {
    let trimmed = strip_leading_trivia(source);
    trimmed.starts_with("\"use server\"") || trimmed.starts_with("'use server'")
}

fn function_body_starts_with_use_server(source: &str, sym: &SymbolInfo) -> bool {
    let end = sym.range.end_byte.min(source.len());
    let start = sym.range.start_byte.min(end);
    let span = &source[start..end];
    let Some(brace_idx) = function_body_open_brace(span) else {
        return false;
    };
    let after = &span[brace_idx + 1..];
    let trimmed = strip_leading_trivia(after);
    trimmed.starts_with("\"use server\"") || trimmed.starts_with("'use server'")
}

fn function_body_open_brace(span: &str) -> Option<usize> {
    let mut stack = Vec::new();
    let mut last_pair_open = None;
    let bytes = span.as_bytes();
    let mut i = 0usize;
    let mut state = ScanState::Normal;
    let mut escaped = false;

    while i < bytes.len() {
        let b = bytes[i];
        match state {
            ScanState::Normal => {
                if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
                    state = ScanState::LineComment;
                    i += 2;
                    continue;
                }
                if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    state = ScanState::BlockComment;
                    i += 2;
                    continue;
                }
                match b {
                    b'\'' => {
                        state = ScanState::SingleString;
                        escaped = false;
                    }
                    b'"' => {
                        state = ScanState::DoubleString;
                        escaped = false;
                    }
                    b'`' => {
                        state = ScanState::TemplateString;
                        escaped = false;
                    }
                    b'{' => stack.push(i),
                    b'}' => {
                        if let Some(open) = stack.pop() {
                            last_pair_open = Some(open);
                        }
                    }
                    _ => {}
                }
            }
            ScanState::LineComment => {
                if b == b'\n' {
                    state = ScanState::Normal;
                }
            }
            ScanState::BlockComment => {
                if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    state = ScanState::Normal;
                    i += 2;
                    continue;
                }
            }
            ScanState::SingleString => {
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'\'' {
                    state = ScanState::Normal;
                }
            }
            ScanState::DoubleString => {
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'"' {
                    state = ScanState::Normal;
                }
            }
            ScanState::TemplateString => {
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'`' {
                    state = ScanState::Normal;
                }
            }
        }

        i += 1;
    }

    last_pair_open
}

#[derive(Clone, Copy)]
enum ScanState {
    Normal,
    LineComment,
    BlockComment,
    SingleString,
    DoubleString,
    TemplateString,
}

/// Skip whitespace, line comments, and block comments at the start of `text`.
/// Doesn't try to be a full lexer ~ enough to skip a file's leading banner /
/// shebang-like prefix before checking the directive.
fn strip_leading_trivia(text: &str) -> &str {
    let mut s = text;
    loop {
        let trimmed = s.trim_start();
        if trimmed.starts_with("//") {
            if let Some(nl) = trimmed.find('\n') {
                s = &trimmed[nl + 1..];
                continue;
            }
            return "";
        }
        if trimmed.starts_with("/*") {
            if let Some(end) = trimmed.find("*/") {
                s = &trimmed[end + 2..];
                continue;
            }
            return "";
        }
        return trimmed;
    }
}

/// True when `sym` is preceded by `export` (optionally followed by `async`).
/// The structural parser's range covers `[async] function name(...) { ... }`
/// without the leading `export` keyword, so we have to look at the bytes
/// before `start_byte` instead of inside the symbol's slice.
fn symbol_is_exported(source: &str, sym: &SymbolInfo) -> bool {
    let prefix_end = sym.range.start_byte.min(source.len());
    if prefix_end == 0 {
        return false;
    }
    let prefix = source[..prefix_end].trim_end();
    prefix.ends_with("export") || prefix.ends_with("export async")
}

fn is_top_level_function(sym: &SymbolInfo) -> bool {
    sym.parent.is_none() && is_function_like(sym)
}

fn is_function_like(sym: &SymbolInfo) -> bool {
    sym.kind == "function" || sym.kind == "method"
}

/// Match top-level exported HTTP verb handlers in either form:
///   `export async function GET(...)`
///   `export const POST = async (...) => ...`
///   `export let PUT: Handler = ...`
/// The parser-emitted symbol list only catches the function form, so this
/// regex backs the const-arrow / let / var forms (which are common in
/// route.ts files).
static EXPORT_VERB_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?m)^\s*export\s+(?:async\s+)?(?:function|const|let|var)\s+(GET|POST|PUT|DELETE|PATCH|HEAD|OPTIONS)\b",
    )
    .expect("EXPORT_VERB_RE compiles")
});

fn extract_methods(file_path: &Path) -> Option<Vec<String>> {
    let source = std::fs::read_to_string(file_path).ok()?;
    extract_methods_from_source(&source)
}

fn extract_methods_from_source(source: &str) -> Option<Vec<String>> {
    let mut methods: Vec<String> = Vec::new();
    for cap in EXPORT_VERB_RE.captures_iter(source) {
        let name = cap.get(1)?.as_str().to_string();
        push_method(&mut methods, &name);
    }
    for export in export_list_entries(source) {
        if HTTP_VERBS.contains(&export.exported.as_str()) {
            push_method(&mut methods, &export.exported);
        }
    }
    if methods.is_empty() {
        return None;
    }
    methods.sort_by_key(|m| {
        HTTP_VERBS
            .iter()
            .position(|v| *v == m.as_str())
            .unwrap_or(usize::MAX)
    });
    Some(methods)
}

fn push_method(methods: &mut Vec<String>, name: &str) {
    if !methods.iter().any(|method| method == name) {
        methods.push(name.to_string());
    }
}

// ~~ shared helpers ~~

fn walk_files(start: &Path) -> Vec<PathBuf> {
    if !start.is_dir() {
        return Vec::new();
    }
    let walker = WalkBuilder::new(start)
        .hidden(true)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .follow_links(false)
        .build();

    walker
        .flatten()
        .filter(|entry| entry.file_type().map_or(false, |ft| ft.is_file()))
        .map(|entry| entry.into_path())
        .collect()
}

fn ext_is_route_source(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| {
            ROUTE_EXTS
                .iter()
                .any(|target| target.eq_ignore_ascii_case(ext))
        })
        .unwrap_or(false)
}

fn has_private_segment(rel: &Path) -> bool {
    rel.components().any(|c| {
        if let Component::Normal(s) = c {
            s.to_string_lossy().starts_with('_')
        } else {
            false
        }
    })
}

fn path_normal_segments(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str().map(|v| v.to_string()),
            _ => None,
        })
        .collect()
}

fn relative_to_repo(full_path: &Path, repo_root: &Path) -> String {
    full_path
        .strip_prefix(repo_root)
        .ok()
        .map(|p| {
            p.components()
                .filter_map(|c| match c {
                    Component::Normal(s) => s.to_str(),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("/")
        })
        .unwrap_or_else(|| full_path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_drops_route_groups() {
        let segs = vec!["(marketing)".to_string(), "about".to_string()];
        let (pat, adv) = compute_app_pattern(&segs);
        assert_eq!(pat, "/about");
        assert!(!adv);
    }

    #[test]
    fn pattern_keeps_dynamic_segments_verbatim() {
        let segs = vec!["users".to_string(), "[id]".to_string()];
        let (pat, _) = compute_app_pattern(&segs);
        assert_eq!(pat, "/users/[id]");
    }

    #[test]
    fn pattern_keeps_catchall_segments_verbatim() {
        let segs = vec!["blog".to_string(), "[...slug]".to_string()];
        let (pat, _) = compute_app_pattern(&segs);
        assert_eq!(pat, "/blog/[...slug]");
    }

    #[test]
    fn pattern_drops_parallel_slot_and_tags_advanced() {
        let segs = vec!["@modal".to_string(), "photos".to_string()];
        let (pat, adv) = compute_app_pattern(&segs);
        assert_eq!(pat, "/photos");
        assert!(adv);
    }

    #[test]
    fn pattern_strips_intercepting_prefix_and_tags_advanced() {
        let segs = vec!["photos".to_string(), "(.)preview".to_string()];
        let (pat, adv) = compute_app_pattern(&segs);
        assert_eq!(pat, "/photos/preview");
        assert!(adv);
    }

    #[test]
    fn pattern_strips_repeated_intercepting_prefixes() {
        let segs = vec!["feed".to_string(), "(..)(..)photo".to_string()];
        let (pat, adv) = compute_app_pattern(&segs);
        assert_eq!(pat, "/feed/photo");
        assert!(adv);
    }

    #[test]
    fn pattern_root_is_slash() {
        let (pat, _) = compute_app_pattern(&[]);
        assert_eq!(pat, "/");
    }

    #[test]
    fn use_server_directive_detection_handles_leading_comments() {
        let src = "// banner\n/* block */\n\"use server\";\n\nexport async function foo() {}\n";
        assert!(file_has_use_server_directive(src));
    }

    #[test]
    fn use_server_directive_negative_when_directive_absent() {
        let src = "import x from 'y';\n\nexport async function foo() {}\n";
        assert!(!file_has_use_server_directive(src));
    }

    #[test]
    fn function_directive_detection_handles_destructured_params() {
        let src = "export async function update({ id }: { id: string }) {\n  \"use server\";\n  return id;\n}\n";
        let sym = whole_source_function_symbol(src, "update");
        assert!(function_body_starts_with_use_server(src, &sym));
    }

    #[test]
    fn function_directive_detection_handles_object_type_params() {
        let src = "export async function update(input: { id: string }) {\n  \"use server\";\n  return input.id;\n}\n";
        let sym = whole_source_function_symbol(src, "update");
        assert!(function_body_starts_with_use_server(src, &sym));
    }

    #[test]
    fn file_level_action_names_include_exported_bindings_and_default() {
        let src = "\"use server\";\n\nexport const createPost = async () => ({ ok: true });\nexport default async function save() {}\n";
        let names = file_level_server_action_names(src, &[]);
        assert!(names.contains(&"createPost".to_string()));
        assert!(names.contains(&"default".to_string()));
        assert!(!names.contains(&"*".to_string()));
    }

    #[test]
    fn file_level_action_names_include_default_identifier_exports() {
        let function_src = "\"use server\";\n\nasync function save() {}\nexport default save;\n";
        let sym = whole_source_function_symbol(function_src, "save");
        let names = file_level_server_action_names(function_src, &[sym]);
        assert!(names.contains(&"default".to_string()));
        assert!(!names.contains(&"save".to_string()));
        assert!(!names.contains(&"*".to_string()));

        let binding_src =
            "\"use server\";\n\nconst save = async () => ({ ok: true });\nexport default save;\n";
        let names = file_level_server_action_names(binding_src, &[]);
        assert!(names.contains(&"default".to_string()));
        assert!(!names.contains(&"save".to_string()));
        assert!(!names.contains(&"*".to_string()));

        let call_src =
            "\"use server\";\n\nconst save = async () => ({ ok: true });\nexport default save();\n";
        let names = file_level_server_action_names(call_src, &[]);
        assert!(names.is_empty());
    }

    #[test]
    fn file_level_action_names_include_export_list_entries() {
        let src = "\"use server\";\n\nasync function save() {}\nconst publish = async () => ({ ok: true });\nexport { save, publish as publishPost, type SavePayload };\n";
        let sym = whole_source_function_symbol(src, "save");
        let names = file_level_server_action_names(src, &[sym]);
        assert!(names.contains(&"save".to_string()));
        assert!(names.contains(&"publishPost".to_string()));
        assert!(!names.contains(&"publish".to_string()));
        assert!(!names.contains(&"SavePayload".to_string()));
        assert!(!names.contains(&"*".to_string()));
    }

    #[test]
    fn route_methods_include_export_list_entries() {
        let src = "async function DELETE() {}\nconst updateUser = async () => new Response('ok');\nexport { DELETE, updateUser as PATCH, type RouteContext };\nexport { handler as GET } from './handlers';\n";
        let methods = extract_methods_from_source(src).unwrap();
        assert_eq!(methods, vec!["GET", "DELETE", "PATCH"]);
    }

    fn whole_source_function_symbol(source: &str, name: &str) -> SymbolInfo {
        SymbolInfo {
            kind: "function".to_string(),
            name: name.to_string(),
            qualname: name.to_string(),
            range: crate::models::RangeInfo {
                start_byte: 0,
                end_byte: source.len(),
                start_line: 1,
                end_line: source.lines().count().max(1),
            },
            parent: None,
        }
    }
}
