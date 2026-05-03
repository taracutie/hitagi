<p align="center">
  <img src="hitagi.png" width="400" />
</p>

<h1 align="center">hitagi!</h1>

<p align="center">
  <em>efficient code search~ ♡ 🎀</em>
</p>

---

`hitagi` is a cli tool that allows coding agents (or humans) to efficiently query information about your codebase.
it's meant for my own personal use so for now it only supports the languages I actively use :p

Commands:

- `outline <PATH>` ~ list every symbol in a file with kind, qualname, and line range.
- `symbol <PATH> <QUALNAME>` ~ read one symbol's source by qualname (or unique leaf name).
- `search <QUERY> [PATHS...]` ~ substring search; results group around the enclosing symbol scope and report the actual match line.
- `read <PATH>` ~ dump a file (or a line slice with `--lines S-E`).
- `find <QUERY> [PATHS...]` ~ locate symbols across the repo by qualname substring (case-insensitive).
- `files [GLOBS...]` ~ list files in the repo (gitignore-aware), optionally filtered by globs.
- `langs` ~ summarise languages present in the repo (file count + line count per language).

Supported languages:

**Parseable** (full `outline` / `symbol` / `find` support):

- Rust ~ `.rs` (functions, structs, enums + variants, traits, mod blocks, impl methods)
- TypeScript ~ `.ts` (classes, interfaces + properties + methods, type aliases, functions, fields)
- TSX ~ `.tsx` (same as TypeScript)
- Python ~ `.py`
- Kotlin ~ `.kt`, `.kts`
- Prisma ~ `.prisma`

**Recognised** (named in `langs`, plaintext-search-able, but no symbol info):

- JSON ~ `.json`, `.jsonc`, `.json5`
- YAML ~ `.yaml`, `.yml`
- TOML ~ `.toml`
- Markdown ~ `.md`, `.markdown`, `.mdx`
- SQL ~ `.sql`
- HTML ~ `.html`, `.htm`
- CSS ~ `.css`, `.scss`, `.sass`, `.less`
- Shell ~ `.sh`, `.bash`, `.zsh`, `.fish`
- Dockerfile ~ filename match (`Dockerfile` / `Containerfile`)

Truly unknown extensions get bucketed as `plaintext` ~ still searchable, just unlabelled.

## Install

```bash
cargo install --path .
```

This builds the release binary and drops it at `~/.cargo/bin/hitagi`.

## Usage

`hitagi` defaults to the current working directory as the repo root. Pass `--repo <PATH>` to override.

Paths are repo-relative. If an exact repo-relative path isn't found, path-taking commands fall back to a unique repo-internal suffix ~ e.g. `src-tauri/src/main.rs` resolves to `apps/desktop/src-tauri/src/main.rs` if there's exactly one match. Ambiguous suffixes return an error listing the candidates.

Output is compact JSON to stdout. Pass `--pretty` for indented output. Errors go to stderr with a non-zero exit code.

### `outline <PATH>`

```bash
hitagi outline src/cli.rs --pretty
```

```json
{
  "language": "rust",
  "symbols": [
    { "kind": "struct",   "name": "Cli",      "qualname": "Cli",                "lines": [24, 35] },
    { "kind": "enum",     "name": "Commands", "qualname": "Commands",           "lines": [38, 119] },
    { "kind": "variant",  "name": "Outline",  "qualname": "Commands.Outline",   "lines": [40, 49] }
  ]
}
```

Bodyless `mod foo;` declarations are intentionally omitted (they're imports, not scoped containers); `mod foo { ... }` blocks are included.

Flags:

- `--bytes` ~ also include `bytes: [start, end]` byte offsets per symbol (off by default; agents almost never need them).
- `--kind K1,K2,...` ~ keep only symbols of these kinds. Comma-separated, case-insensitive. Common kinds: `function`, `method`, `struct`, `enum`, `variant`, `class`, `interface`, `property`, `trait`, `module`, `model`, `field`. When no symbol matches, the response includes `available_kinds: [...]` listing what the file actually contains.
- `--depth N` ~ limit nesting depth: `--depth 1` keeps top-level symbols only, `--depth 2` adds one nested level (e.g. methods inside a class, variants inside an enum). Counted by dots in the qualname. Useful for orientation on big files.

### `symbol <PATH> <QUALNAME>`

```bash
hitagi symbol src/lang.rs Language.detect
```

`QUALNAME` accepts the full dotted form (e.g. `AuthService.handleAuth`) or just the leaf name (`handleAuth`) when it resolves uniquely within the file. Ambiguous leaves return an error listing the candidates; misses suggest near-miss qualnames.

Flags: `--bytes` (same as outline).

### `search <QUERY> [PATHS...]`

```bash
hitagi search "tree_sitter::Parser" --snippet --pretty
```

```json
{
  "results": {
    "src/parser.rs": [
      "parse_source(function) @L16 :: let mut parser = tree_sitter::Parser::new();"
    ]
  }
}
```

Each entry follows the format `<scope>(<kind>) @L<match_line>` for matches inside a parsed symbol, or just `@L<match_line>` for matches outside any scope (top-of-file imports, comments, plaintext files). Pass `--snippet` to append the matched line.

Combine alternatives with ` OR ` ~ literal text, surrounded by spaces. `"foo OR bar"` searches for both terms; `"fooORbar"` is a literal substring.

Pass extra positional paths to scope the search:

```bash
hitagi search validateInput src tests
```

Flags:

- `--limit N` ~ maximum total matches to return (default `50`). Response includes `"truncated": true` when the cap is hit.
- `--snippet` ~ append the matched line as ` :: <line text>` (truncated at 100 chars).
- `--exclude PATTERN` (repeatable) ~ skip files matching the pattern. Bare names like `--exclude vendor` skip that directory at any depth; full globs like `--exclude "vendor/**"` work too.

### `read <PATH>`

```bash
hitagi read src/lang.rs
```

Returns `{ "language": "rust", "content": "..." }`. For files with no recognised extension, `language` is `"plaintext"`.

Flags:

- `--lines S-E` ~ slice to a 1-indexed inclusive line range, e.g. `--lines 100-200`. The end clamps to the file; if `S` is past EOF you get an error (`--lines start (X) is past end of file (file has N lines)`). Slicing adds `"lines": [s, e]` and `"total_lines": N` to the response.

### `find <QUERY> [PATHS...]`

```bash
hitagi find load_source --snippet --pretty
```

```json
{
  "matches": [
    {
      "path": "src/commands.rs",
      "kind": "function",
      "name": "load_source",
      "qualname": "load_source",
      "lines": [380, 422],
      "snippet": "fn load_source(resolved: &ResolvedPath) -> AppResult<LoadedSource> {"
    }
  ],
  "searched_files": 18
}
```

Walks the repo, parses every supported file, returns symbols whose qualname contains `QUERY` (case-insensitive). Use this when you know the symbol name but not the file. Only matches qualnames within parseable files; `.md`/`.txt`/etc. are skipped ~ for raw substring search across all files, use `search`.

Pass extra positional paths to scope the walk.

Flags:

- `--limit N` ~ default `50`. Response includes `"truncated": true` when hit.
- `--kind K1,K2,...` ~ case-insensitive symbol-kind filter, same syntax as outline. Empty matches → `available_kinds` hint.
- `--bytes` ~ include byte ranges.
- `--snippet` ~ include each symbol's first-line signature as a `snippet` field.
- `--terse` ~ compact output mode: `matches` becomes a flat list of strings like `"src/foo.rs:42 Foo.bar(method)"` (with snippet appended after ` :: ` if `--snippet` is also passed). ~3x smaller for sweep queries.
- `--exclude PATTERN` (repeatable) ~ skip matching files (same syntax as `search --exclude`).

`searched_files` reports how many parseable files were inspected. When zero (e.g. `find foo vendor`), the response includes a `note` explaining why ~ usually "no parseable files at this path; for plaintext search across all file types, use `search`".

### `files [GLOBS...]`

```bash
hitagi files "src/**/*.rs" "**/*.toml"
```

```json
{
  "files": [
    "Cargo.toml",
    "src/cli.rs",
    "src/commands.rs",
    "src/error.rs",
    "src/lang.rs",
    "src/main.rs",
    "src/models.rs",
    "src/parser.rs",
    "src/queries.rs",
    "src/repo.rs"
  ]
}
```

Lists all files in the repo, sorted alphabetically. Respects `.gitignore` (and `.git/info/exclude`). Pass one or more positional [globset](https://docs.rs/globset/) patterns to filter (multiple are OR'd) ~ `**` for any-depth directory wildcard, `*` for one segment, etc.

Flags:

- `--limit N` ~ maximum number of files to return (default `2000`). Response includes `"truncated": true` and a `"note"` field suggesting how to refine when the cap is hit.
- `--exclude PATTERN` (repeatable) ~ skip files matching the pattern. Bare names like `--exclude vendor` skip that directory at any depth.

### `langs`

```bash
hitagi langs --pretty
```

```json
{
  "languages": [
    { "language": "rust",     "files": 9, "lines": 2400, "parseable": true  },
    { "language": "markdown", "files": 4, "lines": 870,  "parseable": false },
    { "language": "tsx",      "files": 2, "lines": 312,  "parseable": true  }
  ]
}
```

One-shot orientation: walks the repo and tallies file count + line count per detected language. Sorted by file count descending. The `parseable` flag tells you which entries support `outline`/`symbol`/`find` (Rust, TypeScript, TSX, Python, Kotlin, Prisma) ~ the rest are recognised by extension but only respond to `search` and `read`.

## Limits

Built-in caps:

- max file size ~ `1048576` bytes
- max symbol/file response size ~ `262144` bytes (use `--lines` to slice big files)
- search/find default match cap ~ `50` (override with `--limit`)
- files default cap ~ `2000` (override with `--limit`)

Files exceeding these caps return an error rather than truncating.

## Maintenance

The compiled tree-sitter parsers in `vendor/*/src/` are gitignored. To regenerate them after pulling fresh grammar sources:

```bash
for grammar in vendor/tree-sitter-rust vendor/tree-sitter-python vendor/tree-sitter-kotlin vendor/tree-sitter-prisma vendor/tree-sitter-typescript vendor/tree-sitter-tsx; do
  (cd "$grammar" && tree-sitter generate --abi 14)
done
```

Requires the [tree-sitter CLI](https://tree-sitter.github.io/tree-sitter/cli/) on `$PATH`.