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
- `diff [PATH]` ~ review uncommitted changes; overview by default, structured hunks with enclosing-symbol annotation when a path is given.
- `cache [status|path|clear]` ~ inspect or manage the on-disk parse cache.

When a `find`/`search` walk has no positional [PATHS], it visits top-level subdirs round-robin so a `--limit` truncation produces a fair sample across the repo. Pass [PATHS] to opt out and walk in user-supplied order.

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

When matches span multiple top-level dirs with no shared prefix, the response switches to a grouped shape: `{"groups": [{"prefix": "...", "results": {...}}, ...], "results": {}}`. Each group carries its own `prefix` with file keys stripped relative to it ~ avoids repeating long monorepo paths in every key. See "Response shapes" near the end of `--help`.

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
- `--per-file N` ~ cap matches per file at `N` (default `0` = no cap). When set, suppressed match counts are reported in `more_in_file: { "path": <count>, ... }` (top-level on flat responses, inside the containing group on grouped responses). The cap counts toward `--limit` ~ this is a diversity control, not a bypass. Useful when one class with many methods would otherwise eat the whole budget.
- `--exclude PATTERN` (repeatable) ~ skip matching files (same syntax as `search --exclude`).

`searched_files` reports how many parseable files were inspected. When zero (e.g. `find foo vendor`), the response includes a `note` explaining why ~ usually "no parseable files at this path; for plaintext search across all file types, use `search`".

When matches span multiple top-level dirs with no shared prefix, the response switches to a grouped shape: `{"matches": [], "groups": [{"prefix": "...", "matches": [...], "more_in_file": {...}?}, ...]}`. Each group carries its own `prefix` (the longest common prefix within that bucket) with each match's `path` stripped relative to it. The flat-when-shared and grouped-when-spanning behavior keeps the typical case unchanged while saving a lot of bytes when matches scatter across deep monorepo paths.

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

### `diff [PATH]`

Review uncommitted changes (working tree vs `HEAD` by default). Shells out to `git` ~ requires a git repo. With no `PATH`, prints a one-entry-per-file overview; with a `PATH`, prints structured hunks annotated by enclosing symbol.

```bash
hitagi diff --pretty
```

```json
{
  "files": [
    { "path": "src/cli.rs",     "status": "M", "added": 12, "removed": 3, "unstaged": true },
    { "path": "src/git.rs",     "status": "A", "added": 140, "removed": 0, "staged": true, "unstaged": true },
    { "path": "docs/old.md",    "status": "D", "added": 0, "removed": 33, "unstaged": true },
    { "path": "src/renamed.rs", "status": "R", "old_path": "src/orig.rs", "added": 3, "removed": 3, "unstaged": true },
    { "path": "notes.txt",      "status": "?" }
  ]
}
```

Status codes: `M` modified, `A` added, `D` deleted, `R` renamed, `C` copied, `?` untracked. Untracked files have no `added`/`removed` (drilldown isn't supported for them ~ use `read` for content).

```bash
hitagi diff src/cli.rs --pretty
```

```json
{
  "path": "src/cli.rs",
  "status": "M",
  "added": 12,
  "removed": 0,
  "language": "rust",
  "hunks": [
    {
      "old_lines": [320, 320],
      "new_lines": [321, 332],
      "added": 12,
      "removed": 0,
      "symbol": "Commands",
      "kind": "enum",
      "body": "+    /// Show uncommitted changes.\n+    Diff { ... }\n"
    }
  ]
}
```

Each hunk's `symbol` / `kind` is the innermost parsed symbol that contains the hunk (Rust/TS/TSX/Python/Kotlin/Prisma only). Multi-symbol hunks include a `spans: [...]` field listing every overlapping qualname. Pure deletions still get annotated ~ the HEAD-side blob is fetched via `git show` and parsed in-memory (no cache write).

Flags:

- `--symbol QUALNAME` ~ narrow drilldown to hunks overlapping one symbol. Same qualname/leaf semantics as the top-level `symbol` command (suggests near-misses on misspellings).
- `--raw` ~ emit the unified diff text instead of structured hunks. Mutually exclusive with `--symbol`.
- `--staged` ~ index vs base ref only.
- `--unstaged` ~ working tree vs index only.
- `--against REF` ~ compare against `REF` instead of `HEAD`. Validated; rejects leading `-`, `..`, NUL, and whitespace before any subprocess fires.
- `--exclude PATTERN` (repeatable) ~ skip files in the overview. Same syntax as other commands.

Path resolution in drilldown matches against the diff's own file list (not a filesystem walk), so suffix shorthand works the same as `outline`/`symbol`:

```bash
hitagi diff Button.tsx           # resolves like outline does, but only against changed files
hitagi diff deleted_file.rs      # works fine ~ deleted files are still in the diff list
```

Monorepo / repo-subdir scoping: `diff` only ever surfaces changes inside the hitagi `--repo` subtree. When `--repo` is a subdir of a larger git toplevel, sibling-project changes are silently filtered and a top-level `note` reports the count. **Cross-subtree renames are surfaced symmetrically:** the destination subtree sees the file as `A` with a per-file `note` naming the toplevel-relative origin; the source subtree sees a synthesized `D` with a `note` naming the toplevel-relative destination. Both halves are drillable.

Token efficiency: a typical pre-commit review (overview ≈ 0.5 KB + one or two file drilldowns ≈ 2-6 KB each) lands well under raw `git diff HEAD` for the same change set ~ which is the main reason this command exists.

### `cache [status|path|clear]`

`outline`, `symbol`, `search`, and `find` automatically persist the parsed symbols of every file they touch, keyed on `(repo-relative path, mtime, size, language)`. Subsequent invocations stat the same files (~30ms for a 3.6k-file repo), reuse cached symbols when nothing changed, and only re-read + re-parse the few files that actually moved. On a 3.6k-file tree this turns a 3.5s cold sweep into a ~140ms warm one.

Cache file lives at `${HITAGI_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}}/hitagi/<repo-hash>/index.v1.bin` (one file per repo, bincode-serialized). Failures (missing dir, corrupt file, version mismatch) silently fall back to a cold parse ~ a stale cache will never break a command.

```bash
hitagi cache              # alias for `cache status`
hitagi cache status       # full info: size, entry count, language breakdown
hitagi cache path         # just the cache directory for this repo
hitagi cache clear        # delete this repo's cache subdir
hitagi cache clear --all  # nuke every repo's cache
```

`status` (default) returns:

```json
{
  "enabled": true,
  "disabled_via_env": false,
  "current_version": "v1-0.1.0",
  "cache_dir": "/home/user/.cache/hitagi/abc123def4567890",
  "cache_file": "/home/user/.cache/hitagi/abc123def4567890/index.v1.bin",
  "exists": true,
  "size_bytes": 7324880,
  "modified_unix_secs": 1714728000,
  "stored_version": "v1-0.1.0",
  "stored_repo_root": "/home/user/code/myrepo",
  "version_match": true,
  "repo_root_match": true,
  "entry_count": 3201,
  "languages": [
    { "language": "typescript", "files": 1893 },
    { "language": "rust",       "files": 642 },
    { "language": "tsx",        "files": 412 }
  ]
}
```

`version_match`/`repo_root_match` flag stale caches: bumping `Cargo.toml`'s version invalidates everything (cheapest proxy for "visitor logic might have changed"); a `false` `repo_root_match` means a hash collision (run `cache clear` and move on).

Environment variables:

- `HITAGI_NO_CACHE=1` ~ skip both the cache load and the cache save for this invocation. Use it to benchmark the cold path or as a safety hatch when the cache is suspect.
- `HITAGI_CACHE_DIR=/path` ~ override where the cache lives entirely (skips the `XDG_CACHE_HOME`/`HOME` fallback chain). Useful for sandboxed CI runs.

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