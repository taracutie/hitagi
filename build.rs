fn main() {
    let grammars: &[(&str, &str, bool)] = &[
        ("tree_sitter_rust", "vendor/tree-sitter-rust", true),
        (
            "tree_sitter_typescript",
            "vendor/tree-sitter-typescript",
            true,
        ),
        ("tree_sitter_tsx", "vendor/tree-sitter-tsx", true),
        ("tree_sitter_python", "vendor/tree-sitter-python", true),
        ("tree_sitter_kotlin", "vendor/tree-sitter-kotlin", true),
        ("tree_sitter_prisma", "vendor/tree-sitter-prisma", false),
    ];

    for (lib_name, dir, has_scanner) in grammars {
        let mut build = cc::Build::new();
        build.include(format!("{dir}/src"));
        build.warnings(false);
        build.file(format!("{dir}/src/parser.c"));
        if *has_scanner {
            build.file(format!("{dir}/scanner.c"));
        }
        build.compile(lib_name);
    }
}
