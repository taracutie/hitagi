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
    fn unsupported_language_errors() {
        let err = parse_source(&Language::Plaintext, "hello").unwrap_err();
        assert!(err.to_string().contains("no parser"));
    }
}
