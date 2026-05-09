use tree_sitter_language_pack::{
    ProcessConfig, Span, StructureItem, StructureKind, SymbolInfo as PackSymbolInfo, SymbolKind,
};

use crate::{
    error::{AppError, AppResult},
    lang::Language,
    models::{RangeInfo, SymbolInfo},
};

#[derive(Debug, Clone)]
pub struct ParsedFile {
    pub symbols: Vec<SymbolInfo>,
}

pub fn parse_source(language: &Language, source: &str) -> AppResult<ParsedFile> {
    if !language.is_parseable() {
        return Err(AppError::unsupported(format!(
            "no parser for {}",
            language.as_str()
        )));
    }

    let config = ProcessConfig::new(language.as_str()).minimal();
    let processed = tree_sitter_language_pack::process(
        source,
        &ProcessConfig {
            structure: true,
            symbols: true,
            ..config
        },
    )
    .map_err(|error| {
        AppError::parse(format!(
            "failed to parse {} with tree-sitter-language-pack: {error}",
            language.as_str()
        ))
    })?;

    let mut symbols = Vec::new();
    for item in &processed.structure {
        flatten_structure(item, None, &mut symbols);
    }
    for symbol in &processed.symbols {
        push_pack_symbol(symbol, &mut symbols);
    }
    if language.as_str() == "kotlin" {
        push_kotlin_symbols(source, &mut symbols)?;
    }
    symbols.sort_by_key(|symbol| (symbol.range.start_byte, symbol.range.end_byte));

    Ok(ParsedFile { symbols })
}

fn push_pack_symbol(symbol: &PackSymbolInfo, symbols: &mut Vec<SymbolInfo>) {
    if symbol.name.is_empty() {
        return;
    }

    let range = range_info(&symbol.span);
    if symbols.iter().any(|existing| {
        existing.range.start_byte == range.start_byte && existing.range.end_byte == range.end_byte
    }) {
        return;
    }

    symbols.push(SymbolInfo {
        kind: symbol_kind_label(&symbol.kind).to_string(),
        name: symbol.name.clone(),
        qualname: symbol.name.clone(),
        range,
        parent: None,
    });
}

fn flatten_structure(item: &StructureItem, parent: Option<&str>, symbols: &mut Vec<SymbolInfo>) {
    let Some(name) = item.name.as_deref().filter(|name| !name.is_empty()) else {
        for child in &item.children {
            flatten_structure(child, parent, symbols);
        }
        return;
    };

    let qualname = match parent {
        Some(parent) if !parent.is_empty() => format!("{parent}.{name}"),
        _ => name.to_string(),
    };
    symbols.push(SymbolInfo {
        kind: structure_kind_label(&item.kind).to_string(),
        name: name.to_string(),
        qualname: qualname.clone(),
        range: range_info(&item.span),
        parent: parent.map(str::to_string),
    });

    for child in &item.children {
        flatten_structure(child, Some(&qualname), symbols);
    }
}

fn structure_kind_label(kind: &StructureKind) -> &str {
    match kind {
        StructureKind::Function => "function",
        StructureKind::Method => "method",
        StructureKind::Class => "class",
        StructureKind::Struct => "struct",
        StructureKind::Interface => "interface",
        StructureKind::Enum => "enum",
        StructureKind::Module => "module",
        StructureKind::Trait => "trait",
        StructureKind::Impl => "impl",
        StructureKind::Namespace => "namespace",
        StructureKind::Other(label) => label.as_str(),
    }
}

fn symbol_kind_label(kind: &SymbolKind) -> &str {
    match kind {
        SymbolKind::Variable => "variable",
        SymbolKind::Constant => "constant",
        SymbolKind::Function => "function",
        SymbolKind::Class => "class",
        SymbolKind::Type => "type",
        SymbolKind::Interface => "interface",
        SymbolKind::Enum => "enum",
        SymbolKind::Module => "module",
        SymbolKind::Other(label) => label.as_str(),
    }
}

fn range_info(span: &Span) -> RangeInfo {
    RangeInfo {
        start_byte: span.start_byte,
        end_byte: span.end_byte,
        start_line: span.start_line + 1,
        end_line: span.end_line + 1,
    }
}

fn push_kotlin_symbols(source: &str, symbols: &mut Vec<SymbolInfo>) -> AppResult<()> {
    let mut parser = tree_sitter_language_pack::get_parser("kotlin").map_err(|error| {
        AppError::parse(format!(
            "failed to initialize kotlin parser with tree-sitter-language-pack: {error}"
        ))
    })?;
    let tree = parser.parse(source, None).ok_or_else(|| {
        AppError::parse("failed to parse kotlin with tree-sitter-language-pack")
    })?;
    if tree.root_node().has_error() {
        return Err(AppError::parse(
            "failed to parse kotlin with tree-sitter-language-pack",
        ));
    }
    collect_kotlin_symbols(tree.root_node(), source, None, symbols);
    Ok(())
}

fn collect_kotlin_symbols(
    node: tree_sitter::Node<'_>,
    source: &str,
    parent: Option<&str>,
    symbols: &mut Vec<SymbolInfo>,
) {
    match node.kind() {
        "class_declaration" => {
            let Some(name) = first_direct_child_text(node, source, &["type_identifier"]) else {
                collect_kotlin_children(node, source, parent, symbols);
                return;
            };
            let kind = kotlin_class_kind(node);
            let qualname = qualify(parent, &name);
            push_symbol_if_missing(symbols, kind, &name, &qualname, parent, node);
            collect_kotlin_children(node, source, Some(&qualname), symbols);
        }
        "object_declaration" => {
            let Some(name) = first_direct_child_text(node, source, &["type_identifier"]) else {
                collect_kotlin_children(node, source, parent, symbols);
                return;
            };
            let qualname = qualify(parent, &name);
            push_symbol_if_missing(symbols, "object", &name, &qualname, parent, node);
            collect_kotlin_children(node, source, Some(&qualname), symbols);
        }
        "companion_object" => {
            let name = first_direct_child_text(node, source, &["type_identifier"])
                .unwrap_or_else(|| "Companion".to_string());
            let qualname = qualify(parent, &name);
            push_symbol_if_missing(symbols, "object", &name, &qualname, parent, node);
            collect_kotlin_children(node, source, Some(&qualname), symbols);
        }
        "function_declaration" => {
            if let Some(name) = first_direct_child_text(node, source, &["simple_identifier"]) {
                let qualname = qualify(parent, &name);
                let kind = if parent.is_some() {
                    "method"
                } else {
                    "function"
                };
                push_symbol_if_missing(symbols, kind, &name, &qualname, parent, node);
            }
        }
        "property_declaration" => {
            if let Some(name) = kotlin_property_name(node, source) {
                let qualname = qualify(parent, &name);
                push_symbol_if_missing(symbols, "property", &name, &qualname, parent, node);
            }
        }
        "enum_entry" => {
            if let Some(name) = first_direct_child_text(node, source, &["simple_identifier"]) {
                let qualname = qualify(parent, &name);
                push_symbol_if_missing(symbols, "variant", &name, &qualname, parent, node);
            }
        }
        _ => collect_kotlin_children(node, source, parent, symbols),
    }
}

fn collect_kotlin_children(
    node: tree_sitter::Node<'_>,
    source: &str,
    parent: Option<&str>,
    symbols: &mut Vec<SymbolInfo>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_kotlin_symbols(child, source, parent, symbols);
    }
}

fn kotlin_class_kind(node: tree_sitter::Node<'_>) -> &'static str {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            match child.kind() {
                "interface" => return "interface",
                "enum" => return "enum",
                _ => {}
            }
        }
    }
    "class"
}

fn kotlin_property_name(node: tree_sitter::Node<'_>, source: &str) -> Option<String> {
    let variable = first_direct_child(node, "variable_declaration")?;
    first_direct_child_text(variable, source, &["simple_identifier"])
}

fn first_direct_child<'a>(
    node: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.is_named() && child.kind() == kind {
            return Some(child);
        }
    }
    None
}

fn first_direct_child_text(
    node: tree_sitter::Node<'_>,
    source: &str,
    kinds: &[&str],
) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.is_named() && kinds.iter().any(|kind| *kind == child.kind()) {
            let text = child.utf8_text(source.as_bytes()).ok()?;
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }
    }
    None
}

fn qualify(parent: Option<&str>, name: &str) -> String {
    match parent {
        Some(parent) if !parent.is_empty() => format!("{parent}.{name}"),
        _ => name.to_string(),
    }
}

fn push_symbol_if_missing(
    symbols: &mut Vec<SymbolInfo>,
    kind: &str,
    name: &str,
    qualname: &str,
    parent: Option<&str>,
    node: tree_sitter::Node<'_>,
) {
    let range = node_range_info(node);
    if symbols.iter().any(|existing| {
        existing.range.start_byte == range.start_byte && existing.range.end_byte == range.end_byte
    }) {
        return;
    }

    symbols.push(SymbolInfo {
        kind: kind.to_string(),
        name: name.to_string(),
        qualname: qualname.to_string(),
        range,
        parent: parent.map(str::to_string),
    });
}

fn node_range_info(node: tree_sitter::Node<'_>) -> RangeInfo {
    RangeInfo {
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_function_is_extracted_from_pack_structure() {
        let parsed = parse_source(&Language::new("rust"), "fn main() {}\n").unwrap();
        assert!(parsed
            .symbols
            .iter()
            .any(|symbol| symbol.kind == "function" && symbol.qualname == "main"));
    }

    #[test]
    fn go_function_is_pack_parseable() {
        let parsed = parse_source(&Language::new("go"), "package main\nfunc main() {}\n").unwrap();
        assert!(parsed
            .symbols
            .iter()
            .any(|symbol| symbol.kind == "function" && symbol.qualname == "main"));
    }

    #[test]
    fn kotlin_symbols_are_extracted_from_positional_names() {
        let source = r#"
fun topLevel(arg: Int) = arg + 1

data class User(val id: String) {
    companion object {
        const val TAG = "User"
        private fun make(): User = User(TAG)
    }
}

interface Scanner {
    fun scan(value: String): Boolean
}

enum class Mode { FAST, SLOW }
"#;
        let parsed = parse_source(&Language::new("kotlin"), source).unwrap();

        assert_symbol(&parsed, "function", "topLevel");
        assert_symbol(&parsed, "class", "User");
        assert_symbol(&parsed, "object", "User.Companion");
        assert_symbol(&parsed, "property", "User.Companion.TAG");
        assert_symbol(&parsed, "method", "User.Companion.make");
        assert_symbol(&parsed, "interface", "Scanner");
        assert_symbol(&parsed, "method", "Scanner.scan");
        assert_symbol(&parsed, "enum", "Mode");
        assert_symbol(&parsed, "variant", "Mode.FAST");
        assert_symbol(&parsed, "variant", "Mode.SLOW");
    }

    #[test]
    fn unsupported_language_errors() {
        let err = parse_source(&Language::Plaintext, "hello").unwrap_err();
        assert!(err.to_string().contains("no parser"));
    }

    fn assert_symbol(parsed: &ParsedFile, kind: &str, qualname: &str) {
        assert!(
            parsed
                .symbols
                .iter()
                .any(|symbol| symbol.kind == kind && symbol.qualname == qualname),
            "missing {kind} {qualname}; got {:?}",
            parsed.symbols
        );
    }
}
