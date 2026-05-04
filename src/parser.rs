use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::collections::HashMap;

use tree_sitter::Node;

use crate::{
    error::{AppError, AppResult},
    lang::Language,
    models::{RangeInfo, SymbolInfo},
};

#[derive(Debug, Clone)]
pub struct ParsedFile {
    pub language: Language,
    pub symbols: Vec<SymbolInfo>,
}

thread_local! {
    static PARSERS: RefCell<HashMap<Language, tree_sitter::Parser>> =
        RefCell::new(HashMap::new());
}

pub fn parse_source(language: Language, source: &str) -> AppResult<ParsedFile> {
    let tree_sitter_language = language
        .tree_sitter_language()
        .ok_or_else(|| AppError::unsupported(format!("no parser for {}", language.as_str())))?;

    let tree = PARSERS.with(|cell| -> AppResult<tree_sitter::Tree> {
        let mut map = cell.borrow_mut();
        let parser = match map.entry(language) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                let mut p = tree_sitter::Parser::new();
                p.set_language(&tree_sitter_language).map_err(|error| {
                    AppError::parse(format!("failed to configure parser: {error}"))
                })?;
                e.insert(p)
            }
        };
        parser
            .parse(source, None)
            .ok_or_else(|| AppError::parse("tree-sitter failed to parse source"))
    })?;

    let mut symbols = Vec::new();
    let root = tree.root_node();

    match language {
        Language::Rust => visit_rust(root, source, &mut symbols, None),
        Language::TypeScript | Language::Tsx => visit_typescript(root, source, &mut symbols, None),
        Language::Python => visit_python(root, source, &mut symbols, None, false),
        Language::Kotlin => visit_kotlin(root, source, &mut symbols, None, false),
        Language::Prisma => visit_prisma(root, source, &mut symbols, None),
        // Recognized but not parseable ~ already filtered by callers, but be defensive.
        _ => {
            return Err(AppError::unsupported(format!(
                "no parser for {}",
                language.as_str()
            )))
        }
    }

    Ok(ParsedFile { language, symbols })
}

fn visit_rust(node: Node<'_>, source: &str, symbols: &mut Vec<SymbolInfo>, prefix: Option<String>) {
    if node.kind() == "impl_item" {
        let next_prefix = rust_impl_prefix(node, source, prefix.as_deref());
        recurse_rust(node, source, symbols, next_prefix);
        return;
    }

    let mut next_prefix = prefix.clone();
    if let Some((kind, name, is_container)) = rust_symbol(node, source) {
        let qualname = combine_qualname(prefix.as_deref(), &name);
        symbols.push(SymbolInfo {
            kind: kind.to_string(),
            name,
            qualname: qualname.clone(),
            range: range_info(node),
            parent: prefix.clone(),
        });

        if is_container {
            next_prefix = Some(qualname);
        }
    }

    recurse_rust(node, source, symbols, next_prefix);
}

fn recurse_rust(
    node: Node<'_>,
    source: &str,
    symbols: &mut Vec<SymbolInfo>,
    prefix: Option<String>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_rust(child, source, symbols, prefix.clone());
    }
}

fn visit_typescript(
    node: Node<'_>,
    source: &str,
    symbols: &mut Vec<SymbolInfo>,
    prefix: Option<String>,
) {
    let mut next_prefix = prefix.clone();
    if let Some((kind, name, is_container)) = typescript_symbol(node, source) {
        let qualname = combine_qualname(prefix.as_deref(), &name);
        symbols.push(SymbolInfo {
            kind: kind.to_string(),
            name,
            qualname: qualname.clone(),
            range: range_info(node),
            parent: prefix.clone(),
        });

        if is_container {
            next_prefix = Some(qualname);
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        // Inline `{ ... }` type literals (in property annotations, generic args,
        // function signatures, index signatures, etc.) emit their own
        // property_signature nodes that are NOT API surface ~ skipping them
        // here keeps `User.id` from leaking out of `data: { id; handle }`.
        if child.kind() == "object_type" {
            continue;
        }
        visit_typescript(child, source, symbols, next_prefix.clone());
    }
}

fn visit_python(
    node: Node<'_>,
    source: &str,
    symbols: &mut Vec<SymbolInfo>,
    prefix: Option<String>,
    inside_class: bool,
) {
    let mut next_prefix = prefix.clone();
    let mut next_inside_class = inside_class;

    if let Some((kind, name, is_container, container_is_class)) =
        python_symbol(node, source, inside_class)
    {
        let qualname = combine_qualname(prefix.as_deref(), &name);
        symbols.push(SymbolInfo {
            kind: kind.to_string(),
            name,
            qualname: qualname.clone(),
            range: range_info(node),
            parent: prefix.clone(),
        });

        if is_container {
            next_prefix = Some(qualname);
            next_inside_class = container_is_class;
        } else if node.kind() == "function_definition" {
            next_inside_class = false;
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_python(
            child,
            source,
            symbols,
            next_prefix.clone(),
            next_inside_class,
        );
    }
}

fn visit_kotlin(
    node: Node<'_>,
    source: &str,
    symbols: &mut Vec<SymbolInfo>,
    prefix: Option<String>,
    inside_type: bool,
) {
    let mut next_prefix = prefix.clone();
    let mut next_inside_type = inside_type;

    if let Some((kind, name, is_container, container_is_type)) =
        kotlin_symbol(node, source, inside_type)
    {
        let qualname = combine_qualname(prefix.as_deref(), &name);
        symbols.push(SymbolInfo {
            kind: kind.to_string(),
            name,
            qualname: qualname.clone(),
            range: range_info(node),
            parent: prefix.clone(),
        });

        if is_container {
            next_prefix = Some(qualname);
            next_inside_type = container_is_type;
        } else if node.kind() == "function_declaration" {
            next_inside_type = false;
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_kotlin(
            child,
            source,
            symbols,
            next_prefix.clone(),
            next_inside_type,
        );
    }
}

fn rust_symbol(node: Node<'_>, source: &str) -> Option<(&'static str, String, bool)> {
    match node.kind() {
        "function_item" => symbol_name(node, source).map(|name| ("function", name, false)),
        "struct_item" => symbol_name(node, source).map(|name| ("struct", name, false)),
        "enum_item" => symbol_name(node, source).map(|name| ("enum", name, true)),
        "enum_variant" => symbol_name(node, source).map(|name| ("variant", name, false)),
        "trait_item" => symbol_name(node, source).map(|name| ("trait", name, true)),
        "mod_item" => {
            if !rust_mod_has_body(node) {
                return None;
            }
            symbol_name(node, source).map(|name| ("module", name, true))
        }
        _ => None,
    }
}

fn rust_mod_has_body(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    let result = node
        .named_children(&mut cursor)
        .any(|child| child.kind() == "declaration_list");
    result
}

fn rust_impl_prefix(node: Node<'_>, source: &str, prefix: Option<&str>) -> Option<String> {
    let target = node
        .child_by_field_name("type")
        .and_then(|child| node_text(child, source))
        .or_else(|| symbol_name(node, source));

    target.map(|name| combine_qualname(prefix, name.trim()))
}

fn typescript_symbol(node: Node<'_>, source: &str) -> Option<(&'static str, String, bool)> {
    match node.kind() {
        "class_declaration" => symbol_name(node, source).map(|name| ("class", name, true)),
        "interface_declaration" => symbol_name(node, source).map(|name| ("interface", name, true)),
        "type_alias_declaration" => {
            symbol_name(node, source).map(|name| ("type_alias", name, false))
        }
        "function_declaration" => symbol_name(node, source).map(|name| ("function", name, false)),
        "method_definition" => symbol_name(node, source).map(|name| ("method", name, false)),
        "method_signature" => symbol_name(node, source).map(|name| ("method", name, false)),
        "property_signature" => symbol_name(node, source).map(|name| ("property", name, false)),
        "public_field_definition" | "field_definition" => {
            symbol_name(node, source).map(|name| ("field", name, false))
        }
        "variable_declarator" => {
            let value = node.child_by_field_name("value")?;
            match value.kind() {
                "arrow_function" | "function" => {
                    symbol_name(node, source).map(|name| ("function", name, false))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn python_symbol(
    node: Node<'_>,
    source: &str,
    inside_class: bool,
) -> Option<(&'static str, String, bool, bool)> {
    match node.kind() {
        "class_definition" => symbol_name(node, source).map(|name| ("class", name, true, true)),
        "function_definition" => {
            let kind = if inside_class { "method" } else { "function" };
            symbol_name(node, source).map(|name| (kind, name, false, false))
        }
        _ => None,
    }
}

fn kotlin_symbol(
    node: Node<'_>,
    source: &str,
    inside_type: bool,
) -> Option<(&'static str, String, bool, bool)> {
    match node.kind() {
        "class_declaration" => symbol_name(node, source).map(|name| ("class", name, true, true)),
        "object_declaration" => symbol_name(node, source).map(|name| ("object", name, true, true)),
        "interface_declaration" => {
            symbol_name(node, source).map(|name| ("interface", name, true, true))
        }
        "companion_object" => Some(("companion", "companion".to_string(), true, true)),
        "function_declaration" => {
            let kind = if inside_type { "method" } else { "function" };
            symbol_name(node, source).map(|name| (kind, name, false, false))
        }
        _ => None,
    }
}

fn visit_prisma(
    node: Node<'_>,
    source: &str,
    symbols: &mut Vec<SymbolInfo>,
    prefix: Option<String>,
) {
    let mut next_prefix = prefix.clone();
    if let Some((kind, name, is_container)) = prisma_symbol(node, source) {
        let qualname = combine_qualname(prefix.as_deref(), &name);
        symbols.push(SymbolInfo {
            kind: kind.to_string(),
            name,
            qualname: qualname.clone(),
            range: range_info(node),
            parent: prefix.clone(),
        });

        if is_container {
            next_prefix = Some(qualname);
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_prisma(child, source, symbols, next_prefix.clone());
    }
}

fn prisma_symbol(node: Node<'_>, source: &str) -> Option<(&'static str, String, bool)> {
    match node.kind() {
        "model_declaration" => symbol_name(node, source).map(|name| ("model", name, true)),
        "enum_declaration" => symbol_name(node, source).map(|name| ("enum", name, true)),
        "type_declaration" => symbol_name(node, source).map(|name| ("type", name, true)),
        "generator_declaration" => symbol_name(node, source).map(|name| ("generator", name, true)),
        "datasource_declaration" => {
            symbol_name(node, source).map(|name| ("datasource", name, true))
        }
        "view_declaration" => symbol_name(node, source).map(|name| ("view", name, true)),
        "column_declaration" => symbol_name(node, source).map(|name| ("field", name, false)),
        "enumeral" => symbol_name(node, source)
            .or_else(|| node_text(node, source).map(|t| t.trim().to_string()))
            .filter(|name| !name.is_empty())
            .map(|name| ("value", name, false)),
        _ => None,
    }
}

fn symbol_name(node: Node<'_>, source: &str) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|child| node_text(child, source))
        .or_else(|| {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                match child.kind() {
                    "identifier"
                    | "type_identifier"
                    | "property_identifier"
                    | "simple_identifier" => return node_text(child, source),
                    _ => {}
                }
            }
            None
        })
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

fn combine_qualname(prefix: Option<&str>, name: &str) -> String {
    match prefix {
        Some(prefix) if !prefix.is_empty() => format!("{prefix}.{name}"),
        _ => name.to_string(),
    }
}

fn node_text(node: Node<'_>, source: &str) -> Option<String> {
    node.utf8_text(source.as_bytes())
        .ok()
        .map(|text| text.to_string())
}

fn range_info(node: Node<'_>) -> RangeInfo {
    RangeInfo {
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
    }
}

#[cfg(test)]
mod tests {
    use super::parse_source;
    use crate::lang::Language;

    #[test]
    fn typescript_interface_members_extracted() {
        let source = r#"
export interface UserConfig {
  name: string;
  age: number;
  greet(message: string): void;
}
"#;
        let parsed = parse_source(Language::TypeScript, source).unwrap();
        let qualnames: Vec<&str> = parsed.symbols.iter().map(|s| s.qualname.as_str()).collect();
        assert!(qualnames.contains(&"UserConfig"));
        assert!(qualnames.contains(&"UserConfig.name"));
        assert!(qualnames.contains(&"UserConfig.age"));
        assert!(qualnames.contains(&"UserConfig.greet"));

        let kinds: Vec<&str> = parsed
            .symbols
            .iter()
            .filter(|s| s.qualname.starts_with("UserConfig."))
            .map(|s| s.kind.as_str())
            .collect();
        assert!(kinds.contains(&"property"));
        assert!(kinds.contains(&"method"));
    }

    #[test]
    fn rust_enum_variants_extracted() {
        let source = r#"
pub enum Color {
    Red,
    Green,
    Blue,
}
"#;
        let parsed = parse_source(Language::Rust, source).unwrap();
        let qualnames: Vec<&str> = parsed.symbols.iter().map(|s| s.qualname.as_str()).collect();
        assert!(qualnames.contains(&"Color"));
        assert!(qualnames.contains(&"Color.Red"));
        assert!(qualnames.contains(&"Color.Green"));
        assert!(qualnames.contains(&"Color.Blue"));
    }

    #[test]
    fn rust_bodyless_mod_declarations_skipped() {
        let source = r#"
mod foo;
mod bar { pub fn inside() {} }
"#;
        let parsed = parse_source(Language::Rust, source).unwrap();
        let module_names: Vec<&str> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == "module")
            .map(|s| s.qualname.as_str())
            .collect();
        assert_eq!(module_names, vec!["bar"]);
    }

    #[test]
    fn typescript_nested_inline_object_types_do_not_emit_phantom_properties() {
        let source = r#"
interface User {
  name: string;
  data: { id: string; handle: string };
  list: Array<{ key: string; value: number }>;
}
"#;
        let parsed = parse_source(Language::TypeScript, source).unwrap();
        let qualnames: Vec<&str> = parsed.symbols.iter().map(|s| s.qualname.as_str()).collect();

        assert!(qualnames.contains(&"User"));
        assert!(qualnames.contains(&"User.name"));
        assert!(qualnames.contains(&"User.data"));
        assert!(qualnames.contains(&"User.list"));

        // Inner inline-object property names must NOT leak as siblings of
        // the real interface members.
        assert!(!qualnames.contains(&"User.id"));
        assert!(!qualnames.contains(&"User.handle"));
        assert!(!qualnames.contains(&"User.key"));
        assert!(!qualnames.contains(&"User.value"));

        // And not as bare top-level symbols either.
        assert!(!qualnames.contains(&"id"));
        assert!(!qualnames.contains(&"handle"));
        assert!(!qualnames.contains(&"key"));
        assert!(!qualnames.contains(&"value"));
    }

    #[test]
    fn typescript_class_field_with_inline_object_type_does_not_emit_phantoms() {
        let source = r#"
class Handler {
  name: string = "";
  config: { host: string; port: number } = { host: "", port: 0 };
}
"#;
        let parsed = parse_source(Language::TypeScript, source).unwrap();
        let qualnames: Vec<&str> = parsed.symbols.iter().map(|s| s.qualname.as_str()).collect();

        assert!(qualnames.contains(&"Handler"));
        assert!(qualnames.contains(&"Handler.name"));
        assert!(qualnames.contains(&"Handler.config"));

        assert!(!qualnames.contains(&"Handler.host"));
        assert!(!qualnames.contains(&"Handler.port"));
    }

    #[test]
    fn typescript_type_alias_with_inline_object_does_not_leak_top_level_symbols() {
        let source = r#"
type Account = {
  accounts: Array<{ id: string; handle: string }>;
};
"#;
        let parsed = parse_source(Language::TypeScript, source).unwrap();
        let qualnames: Vec<&str> = parsed.symbols.iter().map(|s| s.qualname.as_str()).collect();

        assert!(qualnames.contains(&"Account"));

        // Today's bug emitted these as bare top-level symbols (no Account.
        // prefix because type_alias_declaration is not a container). Skipping
        // object_type recursion eliminates them entirely.
        assert!(!qualnames.contains(&"id"));
        assert!(!qualnames.contains(&"handle"));
        assert!(!qualnames.contains(&"accounts"));
    }
}
