<p align="center">
  <img src="hitagi.png" width="400" />
</p>

<h1 align="center">hitagi!</h1>

<p align="center">
  <em>efficient code search~ ♡ 🎀</em>
</p>

---

`hitagi` is a cli tool that allows coding agents (or humans) to efficiently query information about your codebase.
Tree-sitter language support comes from `tree-sitter-language-pack`; no grammars are vendored in this repo.

Commands:

- `outline <PATH>` ~ list every symbol in a file with kind, qualname, and line range.
- `symbol <PATH> <QUALNAME>` ~ read one symbol's source by qualname (or unique leaf name).
- `search <QUERY> [PATHS...]` ~ ranked hybrid search (BM25 + Model2Vec semantic, RRF-fused chunks).
- `find-related <FILE> <LINE>` ~ semantically related chunks to one you're already looking at.
- `read <PATH>` ~ dump a file, a line slice with `--lines S-E`, or metadata-only structure with `--summary`.
- `find <QUERY> [PATHS...]` ~ locate symbols across the repo by qualname substring (case-insensitive).
- `loc symbols|files` ~ rank symbols or files by language-aware code-line count.
- `files [GLOBS...]` ~ list files in the repo (gitignore-aware), optionally filtered by globs.
- `langs` ~ summarise languages present in the repo (file count + line count per language).
- `diff [PATHS...]` ~ review uncommitted changes; overview by default, `--commit`/`--summary` for compact review, `--paths` for staging lists, structured hunks when file paths are given.
- `cache [status|path|clear]` ~ inspect or manage the on-disk parse cache.
- `index [status|build|clean]` ~ inspect or manage the search index (lives in the same SQLite file as the parse cache; `clean` drops just the search rows).
- `install <claude|codex>` / `uninstall <claude|codex>` ~ add or remove hitagi's user-global agent prompt.

When a `find` walk has no positional [PATHS], it visits top-level subdirs round-robin so a `--limit` truncation produces a fair sample across the repo. `search` always indexes the whole repo (positional [PATHS] is a post-rank filter).

Supported languages are pack-driven:

- Files detected by `tree-sitter-language-pack` are parseable and can support `outline`, `symbol`, `find`, `loc symbols`, syntax-aware `search` chunks, and `diff` symbol annotations.
- `Dockerfile` / `Containerfile` and `Makefile` get explicit filename labels in addition to the pack's path detector.
- Unknown or unsupported files are still available to `read`, `files`, and `langs`, but they are treated as `plaintext` and are not syntax-indexed by `search`.

## Install

```bash
bun run install
```

This builds the release binary and drops it at `~/.cargo/bin/hitagi`.

## Usage

`hitagi` defaults to the current working directory as the repo root. Pass `--repo <PATH>` to override.

Paths are repo-relative. If an exact repo-relative path isn't found, path-taking commands fall back to a unique repo-internal suffix ~ e.g. `src-tauri/src/main.rs` resolves to `apps/desktop/src-tauri/src/main.rs` if there's exactly one match. Ambiguous suffixes return an error listing the candidates.

Output is concise text to stdout. Errors go to stderr with a non-zero exit code.

### Agent prompts

```bash
hitagi install codex
hitagi install claude
```

Installs a small managed instruction block into the agent's user-global prompt file so future sessions run `hitagi --help` first and use `hitagi` for codebase search/read/navigation before falling back to broader tools.

Targets:

- Claude: `~/.claude/CLAUDE.md`
- Codex: `$CODEX_HOME/AGENTS.md` when `CODEX_HOME` is set, otherwise `~/.codex/AGENTS.md`
- Codex override: if `AGENTS.override.md` exists and is non-empty, install writes there because it shadows `AGENTS.md`

Uninstall removes only hitagi's managed block and preserves the rest of the file:

```bash
hitagi uninstall codex
hitagi uninstall claude
```

### `outline <PATH>`

```bash
hitagi outline src/cli.rs
```

```text
outline src/cli.rs
rust • 3/3 symbols
kinds • enum 1 • struct 1 • variant 1
• L24-35 struct Cli
• L38-119 enum Commands
  • L40-49 variant Commands.Outline
```

Bodyless `mod foo;` declarations are intentionally omitted (they're imports, not scoped containers); `mod foo { ... }` blocks are included.

Flags:

- `--bytes` ~ also include `bytes: [start, end]` byte offsets per symbol (off by default; agents almost never need them).
- `--kind K1,K2,...` ~ keep only symbols of these kinds. Comma-separated, case-insensitive. Common kinds: `function`, `method`, `struct`, `enum`, `variant`, `class`, `interface`, `property`, `trait`, `module`, `model`, `field`, `constant`, `variable`. Aliases: `callable` (`function`, `method`, `arrow_function`), `container` (`class`, `struct`, `interface`, `enum`, `trait`, `object`), `value` (`property`, `field`, `variant`, `variable`, `constant`). When no symbol matches, the response includes `available_kinds: [...]` listing what the file actually contains.
- `--depth N` ~ limit nesting depth: `--depth 1` keeps top-level symbols only, `--depth 2` adds one nested level (e.g. methods inside a class, variants inside an enum). Counted by dots in the qualname. Useful for orientation on big files.

### `symbol <PATH> <QUALNAME>`

```bash
hitagi symbol src/lang.rs Language.detect
```

`QUALNAME` accepts the full dotted form (e.g. `AuthService.handleAuth`) or just the leaf name (`handleAuth`) when it resolves uniquely within the file. Ambiguous leaves return an error listing the candidates; misses suggest near-miss qualnames.

Flags: `--bytes` (same as outline).

### `search <QUERY> [PATHS...]`

```bash
hitagi search "where does cache invalidation happen"
```

```text
search "where does cache invalidation happen" • hybrid α=0.65 • 5 hits / 517 chunks in 57 files • 11ms
src/commands.rs:2825-2876   0.0163  hybrid  rust
src/cache.rs:1203-1714      0.0159  hybrid  rust
src/cli.rs:636-663          0.0154  hybrid  rust
README.md:361-410           0.0151  hybrid  markdown
src/repo.rs:1-39            0.0125  hybrid  rust
```

Default mode is **hybrid**: BM25 (lexical) and Model2Vec (semantic) over chunked source, fused with reciprocal rank, with a few generic boosts (symbol-definition match, multi-chunk file rollup, test/compat path penalty).

- `--mode bm25` ~ exact-token / lexical only. No model needed; instant.
- `--mode semantic` ~ embedding-only ranking. Conceptual queries; needs the model.
- `--mode hybrid` ~ default. The auto-tuner picks an alpha based on the query shape (symbol → 0.25, NLQ → 0.65, mixed → 0.45, else 0.55); `--alpha F` overrides.

First call on a repo builds the index ~ a few hundred ms for ~1k files for BM25, plus the embedding pass for hybrid/semantic. Warm calls are ~100 ms. The index lives alongside the parse cache in the same SQLite file (see `index status` / `index clean`).

Flags:

- `-k N` / `--limit N` ~ maximum ranked chunks (default `10`).
- `-m MODE` / `--mode MODE` ~ `hybrid` (default), `bm25`, or `semantic`.
- `--language LANG` (repeatable) ~ restrict to chunks of this language label (`rust`, `go`, ...).
- `--exclude PATTERN` (repeatable) ~ skip files matching the pattern. Bare names like `--exclude vendor` skip that directory at any depth.
- `--alpha F` ~ override the auto-tuned semantic weight (0.0=pure BM25, 1.0=pure semantic).
- `--snippet` ~ append the chunk's first non-blank line as ` :: <line>`.
- `--hashing` ~ use a deterministic hashing encoder instead of Model2Vec. No network, no model file, lower retrieval quality. Useful in CI or when the model isn't available.
- `--no-download` ~ don't download the model if it's missing; use the cached copy or auto-fall back to `--hashing` with a warning.
- `--offline` ~ refuse all network access. Same hashing fallback as `--no-download`.
- `--model PATH_OR_HF_ID` ~ override the default `minishlab/potion-code-16M` model.

Pass positional `[PATHS]` to filter results to chunks under those subtrees:

```bash
hitagi search "queue worker" packages/jobs
```

### `find-related <FILE> <LINE>`

```bash
hitagi find-related src/cli.rs 600
```

Pass a `path:line` from a `search` result to get semantically similar chunks elsewhere in the repo. Reuses the persisted search index; first call rebuilds and may download the model just like `search` does.

Flags: same encoder / model flags as `search` (`--hashing`, `--no-download`, `--offline`, `--model`), plus `-k N`.

### `index [status|build|clean]`

```bash
hitagi index status
hitagi index build --mode hybrid
hitagi index clean
```

Inspect or manage the search index directly. `build` forces a rebuild (handy after a `--model X` swap or to warm a cache before a session). `clean` drops just the search rows (sparse + dense) ~ the parse cache for `outline`/`symbol`/`find` is left intact. Use `cache clear` to wipe both.

### `read <PATH>`

```bash
hitagi read src/lang.rs
```

Prints a short metadata header followed by the file content. For files with no recognised extension, `language` is `"plaintext"`.

Flags:

- `--lines S-E` ~ slice to a 1-indexed inclusive line range, e.g. `--lines 100-200`. The end clamps to the file; if `S` is past EOF you get an error (`--lines start (X) is past end of file (file has N lines)`).
- `--summary` ~ emit metadata, line stats, parseability, and outline symbols without `content`. Useful for untracked/new files when you need structure before deciding what to read.

### `find <QUERY> [PATHS...]`

```bash
hitagi find load_source --snippet
```

```text
find "load_source"
1 matches • 18 files searched
• src/commands.rs:L380-422 function load_source :: fn load_source(resolved: &ResolvedPath) -> AppResult<LoadedSource> {
```

Walks the repo, parses every supported file, returns symbols whose qualname contains `QUERY` (case-insensitive). Use this when you know the symbol name but not the file. Only matches qualnames within parseable files; unsupported plaintext files are skipped.

Pass extra positional paths to scope the walk.

Flags:

- `--limit N` ~ default `50`. Text output notes when the cap is hit.
- `--kind K1,K2,...` ~ case-insensitive symbol-kind filter, same syntax as outline. Empty matches → `available_kinds` hint.
- `--bytes` ~ include byte ranges.
- `--snippet` ~ include each symbol's first-line signature after ` :: `.
- `--terse` ~ compact output mode: match rows become strings like `src/foo.rs:42 Foo.bar(method)` (with snippet appended after ` :: ` if `--snippet` is also passed).
- `--per-file N` ~ cap matches per file at `N` (default `5`; pass `0` for no cap). Suppressed match counts are reported as `… N more in <path>`. The cap counts toward `--limit` ~ this is a diversity control, not a bypass. Useful when one class with many methods would otherwise eat the whole budget.
- `--exclude PATTERN` (repeatable) ~ skip matching files (same syntax as `search --exclude`).

`searched_files` reports how many parseable files were inspected. When zero (e.g. `find foo vendor`), the response includes a `note` explaining why.

When matches span multiple top-level dirs with no shared prefix, text output switches to grouped sections. Each group carries its own prefix with each match's path stripped relative to it. The flat-when-shared and grouped-when-spanning behavior keeps the typical case unchanged while saving a lot of bytes when matches scatter across deep monorepo paths.

### `loc symbols|files`

```bash
hitagi loc symbols --min-lines 80 --snippet
hitagi loc symbols --min-lines 20 --max-lines 80 src
hitagi loc files "**/*.rs" --min-lines 300
```

Ranks parsed symbols or files by language-aware code lines. Code lines are nonblank, noncomment logical lines using the same counter as `langs` and `read --summary`.

`loc symbols` scans parseable files and defaults to `--kind callable` (`function`, `method`, `arrow_function`) so the first results are useful refactoring candidates. It accepts positional `[PATHS]` to scope the scan.

`loc files` scans parseable files only and accepts positional glob patterns like `files`.

Shared flags:

- `--min-lines N` / `--max-lines N` ~ inclusive code-line filters.
- `--limit N` ~ maximum results after sorting (default `50`).
- `--sort code-desc|code-asc|path` ~ default `code-desc`.
- `--language LANG` (repeatable) ~ restrict by detected language label.
- `--exclude PATTERN` (repeatable) ~ skip matching paths.

Symbol-only flags:

- `--kind K1,K2,...` ~ same syntax and aliases as `find`; defaults to `callable`.
- `--bytes` ~ include byte ranges.
- `--snippet` ~ include each symbol's first-line signature.

### `files [GLOBS...]`

```bash
hitagi files "src/**/*.rs" "**/*.toml"
```

```text
files
13 files
• Cargo.toml
• src/cache.rs
• src/cli.rs
• src/commands.rs
• src/error.rs
• src/git.rs
• src/lang.rs
• src/main.rs
• src/models.rs
• src/output.rs
• src/parser.rs
• src/queries.rs
• src/repo.rs
```

Lists all files in the repo, sorted alphabetically. Respects `.gitignore` (and `.git/info/exclude`). Pass one or more positional [globset](https://docs.rs/globset/) patterns to filter (multiple are OR'd) ~ `**` for any-depth directory wildcard, `*` for one segment, etc.

Flags:

- `--limit N` ~ maximum number of files to return (default `2000`). When truncated, text output switches to per-glob or per-root first/last samples.
- `--exclude PATTERN` (repeatable) ~ skip files matching the pattern. Bare names like `--exclude vendor` skip that directory at any depth.

### `langs`

```bash
hitagi langs
```

```text
languages
3 languages
• rust             9 files    2400 lines • parseable
• markdown         4 files     870 lines • plain
• tsx              2 files     312 lines • parseable
```

One-shot orientation: walks the repo and tallies file count + line count per detected language. Sorted by file count descending. The `parseable` flag tells you which entries are supported by `tree-sitter-language-pack` and can produce syntax-aware results for `outline`/`symbol`/`find`/`search`.

### `diff [PATHS...]`

Review uncommitted changes (working tree vs `HEAD` by default). Shells out to `git` ~ requires a git repo. With no `PATH`, prints a one-entry-per-file overview; with a file `PATH`, prints structured hunks annotated by enclosing symbol. Directory paths default to grouped compact summaries.

```bash
hitagi diff
```

```text
diff
5 files
▾ docs/ • 1 file • +0 -33
  └─ D docs/old.md +0 -33 • unstaged

▾ ./ • 1 file
  └─ ? notes.txt

▾ src/ • 3 files • +155 -6
  ├─ M src/cli.rs +12 -3 • unstaged
  ├─ A src/git.rs +140 -0 • staged • unstaged
  └─ R src/renamed.rs +3 -3 ← src/orig.rs • unstaged
```

Status codes: `M` modified, `A` added, `D` deleted, `R` renamed, `C` copied, `?` untracked. Untracked files have no `added`/`removed` in the overview, but path drilldown treats text files as synthetic additions.
Default text overview groups changes by the top-level parent folder in the current repo root and keeps status / staged-state markers on each file line.

```bash
hitagi diff src/cli.rs
```

```text
diff src/cli.rs
M src/cli.rs +12 -0 • rust
@@ -320-320 +321-332 • +12 -0 • Commands(enum)
+    /// Show uncommitted changes.
+    Diff { ... }
```

Each hunk's `symbol` / `kind` is the innermost parsed symbol that contains the hunk (Rust/TS/TSX/Python/Kotlin/Prisma only). Multi-symbol hunks include a `spans: [...]` field listing every overlapping qualname. Pure deletions still get annotated ~ the HEAD-side blob is fetched via `git show` and parsed in-memory (no cache write). Untracked text files are drillable too: they render as synthetic added-file diffs, with symbols parsed from the working-tree file.

```bash
hitagi diff src/cli.rs src/output.rs
```

Multi-file drilldown concatenates file sections in text mode.

```bash
hitagi diff --summary --symbols
```

`--summary` emits compact per-file output for commit review. Add `--symbols` to include touched symbol names without hunk bodies.

```bash
hitagi diff --commit
```

`--commit` is the token-efficient pre-commit preset: compact summary, touched symbols included, no hunk bodies, and grouped text sections for `staged+unstaged`, `staged`, `unstaged`, and `untracked`.

```bash
hitagi diff --paths
```

`--paths` prints one changed repo-relative path per line in text mode. `--names-only` is an alias.

```bash
hitagi diff src tests
```

When every positional path resolves to a directory, plain `diff` returns a grouped summary instead of hunk drilldown. `--summary` and `--commit` also use directory groups when directory paths are passed.

Flags:

- `--symbol QUALNAME` ~ narrow drilldown to hunks overlapping one symbol. Same qualname/leaf semantics as the top-level `symbol` command (suggests near-misses on misspellings).
- `--raw` ~ emit the unified diff text instead of structured hunks. Mutually exclusive with `--symbol`.
- `--summary` ~ emit compact per-file summary output. With no paths, summarizes all visible diff entries; with paths, summarizes only those files.
- `--commit` ~ commit-review preset: summary with touched symbols and grouped state sections.
- `--symbols` ~ `--summary` only: include touched symbols per file, capped to keep output small. `--commit` includes symbols automatically.
- `--paths` / `--names-only` ~ path-only output for staging and commit planning.
- `--body full|changed-lines|added-only|none` ~ structured drilldown body detail. Default is `full`; `none` keeps ranges/symbols without hunk bodies.
- `--snippet` ~ structured drilldown only: append the first changed line to each hunk header.
- `--staged` ~ index vs base ref only.
- `--unstaged` ~ working tree vs index only.
- `--untracked` ~ untracked files only.
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

`outline`, `symbol`, `search`, and `find` automatically persist the parsed symbols of every file they touch, keyed on `(repo-relative path, mtime, size, language)`. Subsequent invocations stat the same files, reuse cached symbols when nothing changed, and only re-read + re-parse the few files that actually moved. Single-file commands fetch just that file's cache row; full-repo walks reuse the same indexed store.

Cache database lives at `${HITAGI_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}}/hitagi/<repo-hash>/index.v3.sqlite` (one SQLite database per repo; symbols are bincode-serialized per file row). Failures (missing dir, corrupt file, version mismatch) silently fall back to a cold parse ~ a stale cache will never break a command.

```bash
hitagi cache              # alias for `cache status`
hitagi cache status       # full info: size, entry count, language breakdown
hitagi cache path         # just the cache directory for this repo
hitagi cache clear        # delete this repo's cache subdir
hitagi cache clear --all  # nuke every repo's cache
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

Language parsers are provided by `tree-sitter-language-pack`; update that crate when parser coverage or grammar versions need to change.
