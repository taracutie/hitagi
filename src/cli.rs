use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::{
    agent_prompt::{self, AgentKind},
    commands::{
        self, DiffBodyMode, DiffFileOptions, DiffOptions, DiffScope, DiffSummaryOptions,
        FilesOptions, FindOptions, FindRelatedOptions, IndexBuildOptions, OutlineOptions,
        ReadOptions, SearchModeArg, SearchOptions, SymbolOptions,
    },
    error::{AppError, AppResult},
    output::{self, OutputMode},
    repo::RepoRoot,
};

const DEFAULT_SEARCH_LIMIT: usize = 10;
const DEFAULT_FIND_RELATED_LIMIT: usize = 10;
const DEFAULT_FIND_LIMIT: usize = 50;
const DEFAULT_PER_FILE: usize = 5;
const DEFAULT_FILES_LIMIT: usize = 2000;

const LONG_ABOUT: &str = "\
hitagi is a local CLI for tree-sitter-backed structural code queries, built for LLM \
coding agents (Claude Code, Codex, etc.) to navigate a codebase token-efficiently. \
Every command parses on demand, prints concise text to stdout, and exits ~ no daemon, \
no auth. The default `search` mode also runs a small embedding model locally for \
hybrid ranking (~30 MB, downloaded on first use; pass `--offline` or `--hashing` to \
skip). Pass --json for machine-readable output.

PRINCIPLE
  Minimize tokens spent reading code. Use outline / find / search to locate the right \
slice first, then read only that slice. Prefer  find -> outline -> symbol  over a raw \
read. Reach for `read` only when you need surrounding context that scope-aware tools \
can't give.

RECOMMENDED WORKFLOW
  1. langs                 one-shot summary of which languages live here
  2. files [GLOBS...]      discover what's in the repo (gitignore-aware, sorted)
  3. find <NAME>           locate a symbol by qualname substring across the repo
  4. outline <FILE>        see the structure of one file (compact, lines only)
  5. symbol <FILE> <Q>     read one symbol's body in isolation
  6. search <Q>            ranked hybrid search (BM25 + semantic, RRF-fused)
  7. find-related <FILE> <LINE>   semantically related chunks
  8. read <FILE>           dump content, line slices, or --summary structure
  9. diff [FILES...]       review changes (overview, --commit, --paths, or drill)

SUPPORTED LANGUAGES
  Language detection and parsing are provided by tree-sitter-language-pack.
  Pack-supported files work with outline / symbol / find and syntax-aware search chunks.
  Unknown or unsupported files are still readable and counted by langs, but search does \
not index them through a plaintext fallback.\
";

const AFTER_LONG_HELP: &str = "\
TIPS

  Token-efficient defaults
    Concise text to stdout by default. Use --json for machine-readable compact
    JSON. Outline omits start_byte/end_byte and parent (derivable from qualname);
    pass --bytes only when you actually need byte offsets.

  Path resolution
    File path is repo-relative (e.g. src/auth.ts) OR a unique repo-internal suffix
    (e.g. src-tauri/src/main.rs auto-resolves to apps/desktop/src-tauri/src/main.rs).
    Ambiguous suffixes error out with the candidates listed.

  Symbol qualnames
    `symbol` accepts the full dotted form (AuthService.handleAuth) or just a unique
    leaf (handleAuth). Misses include near-miss qualnames in the error so you can
    retry without another roundtrip.

  Search ranking
    QUERY can be natural language, a code identifier, or a literal substring. The
    default `--mode hybrid` runs both BM25 (lexical) and Model2Vec (semantic)
    over chunked source, fuses with reciprocal rank, and applies a few generic
    boosts (symbol-definition match, multi-chunk file rollup, test/compat path
    penalty). `--mode bm25` skips the model entirely (instant, no downloads);
    `--mode semantic` uses only the dense index. `--alpha F` overrides the
    auto-tuned semantic weight (0.0=pure BM25, 1.0=pure semantic). Each result
    is a chunk: `path:start-end\\tscore\\tsource\\tlanguage`. Pass [PATHS] to
    scope the search to subtrees; pass `--language LANG` (repeatable) to filter
    by language label.

  Snippets
    `--snippet` on `search` and `find-related` appends ` :: <first non-blank line>`
    of each chunk. `--snippet` on `find` adds the symbol's first-line signature.
    Both save a follow-up `read` when you only needed inline context.

  Embedding model
    First run downloads the default Model2Vec model (~30 MB) under
    $HF_HOME (or $XDG_CACHE_HOME/hitagi/models if HF_HOME is unset). Subsequent
    runs hit the local copy. Pass `--offline` to forbid network entirely and
    fall back to a deterministic hashing encoder (lower quality, no download).
    `--no-download` blocks downloads but allows a cached model. `--model PATH`
    or `--model HF_REPO_ID` overrides the default. The model is fingerprinted
    so swapping invalidates the dense cache row independently of the sparse
    (BM25) row.

  Limits and truncation
    Default `--limit` (`-k`) is 10 for `search` / `find-related`, 50 for `find`,
    2000 for `files`. `find` / `files` carry `\"truncated\": true` when the cap
    is reached and `files` adds per-glob/per-root first/last samples so truncated
    discovery stays useful. `search` / `find-related` always return exactly the
    top-N hits ~ ranking decides what's in vs out; bump `--limit` when you need
    a wider net.

  Fair sampling on full-repo sweeps
    `find` walks top-level subdirs round-robin (one file per bucket in turn)
    when no positional [PATHS] are given, so a `--limit` truncation produces
    a fair sample across the repo instead of exhausting the budget on whichever
    top-level dir comes first alphabetically. Pass [PATHS] to opt out and walk
    in user-supplied order. When matches still don't reach a subtree,
    `unsampled_dirs` lists what was skipped. (`search` is rank-based and
    indexes the whole repo unconditionally; `[PATHS]` is a post-rank filter.)

  Excluding noise (--exclude)
    `search`, `find`, and `files` accept --exclude PATTERN (repeatable). Bare names
    like `--exclude vendor` skip that directory at any depth; full globs like
    `--exclude \"vendor/**\"` work too. Typical: --exclude vendor --exclude target
    --exclude node_modules. Cuts grammar-source / build-artifact noise from sweeps.

  --kind filter
    Case-insensitive (function, Function, FUNCTION all match). When --kind matches
    nothing, the response includes `available_kinds: [...]` so you know what was
    actually present ~ no need for a second probe call. Aliases: callable =
    function/method/arrow_function, container = class/struct/interface/enum/trait/
    object, value = property/field/variant/variable/constant.

  outline --depth N
    Limits nesting depth: --depth 1 keeps top-level shapes only; --depth 2 also
    keeps one level of nesting (e.g. methods inside a class, variants inside an
    enum). Depth is counted from dots in the qualname. Use it on big files where
    you only need orientation.

  read --summary
    Emits language, line stats, parseability, and outline symbols without file
    content. Use it for new/untracked files when raw `read` would spend too many
    tokens before you know which symbol or line range matters.

  find --terse
    Compact output mode. `matches` becomes a list of strings like
    `path:line qualname(kind)` instead of structured objects. Most useful with
    --json or grouped multi-prefix sweeps. With --snippet the line continues
    with ` :: <signature>`.

  find --per-file N
    Cap matches per file at N (default 5; pass 0 for no cap). Useful when one file has
    a class with many methods that all match the query and would otherwise
    eat the global --limit budget. Suppressed match counts are reported per-
    file via `more_in_file: { \"path\": <count>, ... }` (top-level on flat
    responses, inside the containing group on grouped responses). The cap
    counts toward --limit ~ it's a diversity control, not a bypass.

  Per-prefix grouping (find response shape)
    When `find` matches all share a common path prefix, the response stays
    flat: top-level `prefix` plus `matches` with the prefix stripped. When
    matches span multiple top-level dirs and there's no shared prefix, the
    response switches to grouped form: a `groups: [...]` array where each
    group carries its own `prefix` plus its own `matches` (and
    `more_in_file`). Cuts repeated long monorepo paths out of the output.
    `search` / `find-related` always return a flat `results` list (ranking
    is the structure).

  langs and parseable
    `langs` summarises file count + line count per detected language and includes
    `parseable: bool`. Languages with `parseable: true` are supported by
    tree-sitter-language-pack and work with outline/symbol/find/search. Unknown or
    unsupported files only show up in `langs`, `files`, and `read`.

  When `find` returns nothing
    `find` only matches qualnames in PARSEABLE files (.md/.txt/.toml etc. are
    skipped). `search` covers syntax chunks for pack-supported files. The
    `searched_files` field tells you how many files actually got parsed; if it's 0
    the response includes a `note` explaining why.

  Parse cache
    find / outline / symbol persist parsed symbols at
    $HITAGI_CACHE_DIR / $XDG_CACHE_HOME/hitagi / $HOME/.cache/hitagi, keyed by
    (path, mtime, size, language). Warm runs skip the parse step entirely.
    Set HITAGI_NO_CACHE=1 to bypass for one invocation. Use `hitagi cache
    status` to inspect, `hitagi cache clear` to drop the current repo's cache,
    `hitagi cache clear --all` to nuke all of them.

  Search index
    `search` and `find-related` persist their BM25 postings + chunk vector +
    (when hybrid/semantic) dense embeddings in the same SQLite file as the
    parse cache. Cold rebuild on first call (~hundreds of ms for 1000 files;
    longer if the model has to download); warm runs are ~100 ms. The sparse
    and dense rows have independent fingerprints ~ a model swap rebuilds dense
    only; a single file change rebuilds both. Use `hitagi index status` to
    inspect, `hitagi index clean` to drop just the search rows (parse cache
    untouched), `hitagi index build [--mode hybrid]` to force a rebuild.

  Uncommitted changes (`diff`)
    `diff` (no PATH) prints a per-file overview ~ status code (M/A/D/R/C/?),
    ±line counts, staged/unstaged flags, and grouped text sections in the default
    combined scope. `diff <PATH...>` prints structured hunks for one or more
    files; one-path JSON remains the single-file response, multi-path JSON is
    `{ \"files\": [...] }`. Directory PATHS default to grouped summaries. Untracked
    files are drillable as synthetic additions. `--commit` is the pre-commit
    preset: summary + touched symbols + no hunk bodies + grouped text sections.
    `--summary` emits compact per-file output; add `--symbols` to include touched
    symbols. `--paths` / `--names-only` prints one changed path per line. `--body
    full|changed-lines|added-only|none` controls structured hunk bodies, and
    `--snippet` adds the first changed line to each hunk header. Pass --raw for
    unified diff text instead. `--symbol Q` filters a one-file drilldown to hunks
    overlapping one symbol. `--staged` / `--unstaged` / `--untracked` narrow the
    scope. Use `--against REF` to compare against something other than HEAD.
    Deleted files get their HEAD-side blob parsed in-memory so `symbol`
    annotations still appear.

  Monorepo / repo-subdir scoping
    `diff` only ever surfaces changes inside the hitagi `--repo` subtree. When
    `--repo` is itself a subdir of a larger git toplevel (e.g. monorepo with
    sibling projects), changes outside that subtree are silently filtered, and
    a top-level `note` reports the count. Cross-subtree renames are surfaced
    symmetrically: the destination subtree sees the file as `A` (added) with a
    per-file `note` naming the toplevel-relative origin; the source subtree
    sees a synthesized `D` (deleted) entry with a `note` naming the
    toplevel-relative destination. Both halves are drillable. PATH resolution
    in drilldown matches against the diff's own file list (not a filesystem
    walk), so suffix shorthand works (`hitagi diff Button.tsx`) and deleted
    files resolve fine.

COMMON PATTERNS

  What languages are here?        hitagi langs
  Where is symbol X?              hitagi find X --snippet
  What's in this file?            hitagi outline FILE
  Just top-level shapes?          hitagi outline FILE --kind function,struct,enum
  Read this function/struct       hitagi symbol FILE Qualname.Or.Leaf
  Conceptual / NLQ search?        hitagi search \"how does request validation work\"
  Exact-symbol lookup (fast)      hitagi search Foo.bar --mode bm25
  Search a single subtree         hitagi search \"queue worker\" packages/jobs
  Filter to one language          hitagi search \"router\" --language rust
  Sweep without vendor noise      hitagi search \"config\" --exclude vendor --exclude target
  Find related code to a chunk    hitagi find-related src/auth.ts 47
  Index status / lifecycle        hitagi index status / hitagi index clean / hitagi index build
  Offline (no model download)     hitagi search foo --offline
  Read a slice of a big file      hitagi read FILE --lines 1400-1510
  Read structure without content  hitagi read FILE --summary
  List all Rust + TOML files      hitagi files \"**/*.rs\" \"**/*.toml\"
  Find Auth-related classes       hitagi find Auth --kind class,struct --snippet
  Find inside one subtree         hitagi find Network --kind struct src/nnue
  Cheap top-level orientation     hitagi outline FILE --depth 1
  Cheap sweep across the repo     hitagi find X --terse --limit 200
  Diverse sweep (cap hot files)   hitagi find X --terse --per-file 3
  What's uncommitted?             hitagi diff
  Changed paths only              hitagi diff --paths
  Hunks for one file?             hitagi diff src/foo.rs
  Hunks for several files?        hitagi diff src/foo.rs src/bar.rs
  Directory diff summary          hitagi diff src tests
  Diff for one symbol?            hitagi diff src/foo.rs --symbol Foo.bar
  Commit-oriented summary?        hitagi diff --summary --symbols
  Commit-review preset?           hitagi diff --commit
  Ranges without hunk bodies?      hitagi diff src/foo.rs --body none --snippet
  Just staged changes             hitagi diff --staged
  Just untracked changes          hitagi diff --untracked
  Compare against main            hitagi diff --against main

ANTI-PATTERNS (token waste)

  hitagi read big_file.rs                  # ~5K-line files cost a lot of tokens.
                                           # Use `read --summary`, `outline` then
                                           # `symbol`, or `read --lines S-E`.
  hitagi search \"the\"                      # rank quality drops for stopword-only
                                           # queries; pass at least one content
                                           # token, or scope with [PATHS].
  hitagi outline huge_file.rs              # add --kind to filter, or just `find`
                                           # the specific symbol you wanted.
  hitagi outline FILE --bytes              # don't pass --bytes unless you actually
                                           # need byte offsets ~ they ~double the
                                           # output size.

JSON OUTPUT SHAPES (--json; compact form ~ omitted optional fields appear only when set)

  outline   {\"language\":\"rust\",\"symbols\":[{\"kind\":\"...\",\"name\":\"...\",
            \"qualname\":\"...\",\"lines\":[s,e]}],\"available_kinds\":[...]?}
  symbol    {\"language\":\"rust\",\"symbol\":{\"kind\":\"...\",\"name\":\"...\",
            \"qualname\":\"...\",\"content\":\"...\",\"lines\":[s,e]}}
  search    {\"query\":\"...\",\"mode\":\"hybrid|bm25|semantic\",\"alpha\":F,
            \"limit\":N,\"languages\":[...]?,\"paths\":[...]?,\"elapsed_ms\":N,
            \"indexed_files\":N,\"indexed_chunks\":N,\"warnings\":[...]?,
            \"results\":[{\"path\":\"...\",\"lines\":[s,e],\"language\":\"...\"?,
            \"score\":F,\"source\":\"bm25|semantic|hybrid\",
            \"snippet\":\"...\"?}]}
  find-related  {\"path\":\"...\",\"line\":N,\"limit\":N,\"elapsed_ms\":N,
            \"indexed_files\":N,\"indexed_chunks\":N,\"source_chunk\":{...hit...},
            \"warnings\":[...]?,\"results\":[{...hit...},...]}
  index status  {\"cache_file\":\"...\",\"sparse_present\":bool,\"dense_present\":bool,
            \"indexed_files\":N,\"indexed_chunks\":N,\"languages\":{lang:N,...},
            \"model_id\":\"...\"?,\"encoder_kind\":\"...\"?,
            \"model_fingerprint\":\"...\"?,\"dim\":N?,
            \"sparse_built_at_unix_secs\":N?,\"dense_built_at_unix_secs\":N?,
            \"sparse_size_bytes\":N?,\"dense_size_bytes\":N?}
  read      content: {\"language\":\"rust\",\"content\":\"...\",\"lines\":[s,e],
            \"total_lines\":N}
            summary: {\"language\":\"rust\",\"lines\":N,\"bytes\":N,\"parseable\":bool,
            \"total_symbols\":N,\"symbols\":[...]}
  find      {\"prefix\":\"src/\"?,\"matches\":[{\"path\":\"...\",\"kind\":\"...\",
            \"name\":\"...\",\"qualname\":\"...\",\"lines\":[s,e]}],
            \"more_in_file\":{\"path\":N,...}?,\"truncated\":bool,
            \"searched_files\":N,\"available_kinds\":[...]?,\"note\":\"...\"?}
            grouped (when matches span top-levels with no shared prefix):
            {\"matches\":[],\"groups\":[{\"prefix\":\"a/\",\"matches\":[...],
            \"more_in_file\":{...}?},...],\"truncated\":bool,
            \"searched_files\":N,...}
  files     {\"files\":[\"a\",\"b\",...],\"truncated\":bool,
            \"groups\":[{\"pattern\":\"...\"?,\"root\":\"...\"?,\"total\":N,
            \"shown\":N,\"first\":[...],\"last\":[...]}]?,\"note\":\"...\"?}
  langs     {\"languages\":[{\"language\":\"rust\",\"files\":N,\"lines\":N,
            \"parseable\":bool},...]}
  diff      overview: {\"prefix\":\"...\"?,\"files\":[{\"path\":\"...\",
            \"status\":\"M|A|D|R|C|?\",\"old_path\":\"...\"?,\"added\":N?,
            \"removed\":N?,\"staged\":bool?,\"unstaged\":bool?,\"binary\":
            bool?,\"note\":\"...\"?},...],\"against\":\"...\"?,
            \"scope\":\"staged|unstaged|untracked\"?,\"clean\":bool?,\"note\":\"...\"?}
            drilldown: {\"path\":\"...\",\"status\":\"...\",\"old_path\":\"...\"?,
            \"added\":N?,\"removed\":N?,\"language\":\"...\"?,\"hunks\":
            [{\"old_lines\":[s,e],\"new_lines\":[s,e],\"added\":N,\"removed\":N,
            \"symbol\":\"...\"?,\"kind\":\"...\"?,\"spans\":[\"...\"]?,
            \"snippet\":\"...\"?,\"body\":\"...\"?}],\"raw\":\"...\"?,\"binary\":
            bool?,\"note\":\"...\"?}
            multi-drilldown: {\"files\":[{...drilldown...},...]}
            paths: {\"paths\":[\"a\",\"b\",...],\"scope\":\"...\"?,\"against\":\"...\"?}
            summary: {\"files\":[{\"path\":\"...\",\"status\":\"...\",\"added\":N?,
            \"removed\":N?,\"language\":\"...\"?,\"symbols\":[\"...\"]?,
            \"more_symbols\":N?}],\"groups\":[{\"path\":\"...\",\"file_count\":N,
            \"added\":N,\"removed\":N,\"files\":[...]}]?,\"commit\":bool?,
            \"scope\":\"...\"?,\"against\":\"...\"?}

  find --terse override:
    matches (and each group's matches when grouped) becomes a flat list of
    strings like `\"src/foo.rs:42 Foo.bar(method) :: pub fn bar(...) {\"`.

ERRORS
  Errors print to stderr as `error: <msg>` and exit 1. Path-not-found, ambiguous
  suffix, symbol-not-found (with suggestions), --limit < 1, invalid --lines range,
  invalid glob, file too large (>1 MiB), and binary/UTF-8 issues all surface this
  way.\
";

#[derive(Parser)]
#[command(
    name = "hitagi",
    version,
    about = "Local CLI for tree-sitter-backed structural code queries.",
    long_about = LONG_ABOUT,
    after_long_help = AFTER_LONG_HELP,
)]
struct Cli {
    /// Repo root to query. Defaults to the current working directory.
    #[arg(long, global = true, value_name = "PATH")]
    repo: Option<PathBuf>,

    /// Emit compact JSON instead of the default concise text output.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List symbols in a file.
    Outline {
        /// File path relative to the repo root (or a unique repo-internal suffix).
        path: String,
        /// Include byte ranges (`bytes: [start, end]`) on each symbol.
        #[arg(long)]
        bytes: bool,
        /// Filter to symbols of these kinds. Comma-separated, e.g. `--kind function,struct`.
        #[arg(long, value_delimiter = ',', value_name = "KIND")]
        kind: Vec<String>,
        /// Limit nesting depth: 1 = top-level only, 2 = top + 1 nested, etc. Counted by
        /// dots in the qualname (`Foo.bar` has depth 2).
        #[arg(long, value_name = "N")]
        depth: Option<usize>,
    },
    /// Show a single symbol's source by qualified name.
    ///
    /// QUALNAME accepts the full dotted form (e.g. `AuthService.handleAuth`) or just the
    /// leaf name (e.g. `handleAuth`) when it resolves uniquely within the file.
    Symbol {
        /// File path relative to the repo root (or a unique repo-internal suffix).
        path: String,
        /// Qualified symbol name, or a unique leaf name within the file.
        qualname: String,
        /// Include byte ranges (`bytes: [start, end]`) on the symbol.
        #[arg(long)]
        bytes: bool,
    },
    /// Hybrid ranked search across the repo (BM25 + semantic, RRF-fused).
    ///
    /// QUERY can be natural language (`how does request validation work`), a
    /// code identifier (`AuthService.handleAuth`), or a literal substring.
    /// Defaults to hybrid mode; pass `--mode bm25` for exact-token / lexical
    /// search. Each result is a chunk: `path:start-end` with a score and the
    /// fusion source (`bm25` / `semantic` / `hybrid`).
    Search {
        /// Natural-language, code, or literal query.
        query: String,
        /// Optional path prefixes to scope the search to a subtree.
        paths: Vec<String>,
        /// Maximum ranked chunks to return.
        #[arg(short = 'k', long = "limit", default_value_t = DEFAULT_SEARCH_LIMIT)]
        limit: usize,
        /// Ranking mode: hybrid (default), bm25, or semantic.
        #[arg(short = 'm', long, value_enum, default_value_t = CliSearchMode::Hybrid)]
        mode: CliSearchMode,
        /// Restrict to chunks of this language label (`rust`, `go`, ...).
        /// Repeatable.
        #[arg(long = "language", value_name = "LANG")]
        languages: Vec<String>,
        /// Glob patterns to exclude (repeatable). Bare names like `vendor`
        /// exclude that directory at any depth; use `vendor/**` for explicit
        /// globbing.
        #[arg(long, value_name = "PATTERN")]
        exclude: Vec<String>,
        /// Override the auto-tuned hybrid alpha (semantic weight, 0.0-1.0).
        #[arg(long, value_name = "F")]
        alpha: Option<f32>,
        /// Append the chunk's first non-blank line as a snippet (` :: <line>`).
        #[arg(long)]
        snippet: bool,
        /// Use a deterministic hashing encoder instead of model2vec. No
        /// network, no model file, lower retrieval quality.
        #[arg(long)]
        hashing: bool,
        /// Don't download the model if it's missing; use the cached copy or
        /// fail.
        #[arg(long = "no-download")]
        no_download: bool,
        /// Refuse all network access (model download AND any future remote
        /// source). Implies `--no-download`.
        #[arg(long)]
        offline: bool,
        /// Override the embedding model id or local path.
        #[arg(long, value_name = "MODEL")]
        model: Option<String>,
    },
    /// Find chunks related to a known `path:line` by semantic similarity.
    ///
    /// Pass a path:line copied from a `search` result. Reuses the persisted
    /// search index; first run rebuilds (or downloads the model) like
    /// `search` does.
    FindRelated {
        /// Repo-relative file path (or unique repo-internal suffix).
        path: String,
        /// 1-based line inside the source chunk.
        line: usize,
        /// Maximum related chunks to return.
        #[arg(short = 'k', long = "limit", default_value_t = DEFAULT_FIND_RELATED_LIMIT)]
        limit: usize,
        #[arg(long)]
        hashing: bool,
        #[arg(long = "no-download")]
        no_download: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long, value_name = "MODEL")]
        model: Option<String>,
    },
    /// Inspect or manage the search index (lives in the same SQLite file as
    /// the parse cache).
    Index {
        #[command(subcommand)]
        action: Option<IndexAction>,
    },
    /// Read a file's contents.
    Read {
        /// File path relative to the repo root (or a unique repo-internal suffix).
        path: String,
        /// Slice to a 1-indexed inclusive line range, e.g. `--lines 100-200`.
        #[arg(long, value_name = "S-E")]
        lines: Option<String>,
        /// Emit metadata and outline symbols without file content.
        #[arg(long)]
        summary: bool,
    },
    /// Find symbols across the repo whose qualname contains QUERY (case-insensitive).
    ///
    /// Only matches qualnames within parseable files; `.md`/`.txt`/etc. are skipped.
    /// For raw substring search across all file types, use `search`.
    Find {
        /// Substring matched against symbol qualnames (case-insensitive).
        query: String,
        /// Optional path prefixes to scope the find.
        paths: Vec<String>,
        /// Filter to symbols of these kinds (case-insensitive). Comma-separated.
        /// Aliases: callable, container, value.
        #[arg(long, value_delimiter = ',', value_name = "KIND")]
        kind: Vec<String>,
        /// Maximum total matches to return. Response includes `truncated: true` when hit.
        #[arg(long, default_value_t = DEFAULT_FIND_LIMIT)]
        limit: usize,
        /// Include byte ranges (`bytes: [start, end]`) on each match.
        #[arg(long)]
        bytes: bool,
        /// Include the symbol's first-line signature as a `snippet` field on each match.
        #[arg(long)]
        snippet: bool,
        /// Compact output mode: `matches` becomes a list of `"path:line qualname(kind)"`
        /// strings instead of structured objects.
        #[arg(long)]
        terse: bool,
        /// Cap matches per file at N (0 = no cap, default 5). When the cap is hit,
        /// the count of suppressed matches per file is reported in `more_in_file`.
        /// Counted toward --limit ~ this is a diversity control, not a bypass.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_PER_FILE)]
        per_file: usize,
        /// Glob patterns to exclude (repeatable). Bare names like `vendor` exclude that
        /// directory at any depth; use `vendor/**` for explicit globbing.
        #[arg(long, value_name = "PATTERN")]
        exclude: Vec<String>,
    },
    /// List files in the repo (gitignore-aware), optionally filtered by globs.
    ///
    /// Multiple positional GLOBS are OR'd together: `hitagi files "**/*.rs" "**/*.toml"`.
    Files {
        /// Glob patterns. Multiple are OR'd. If omitted, lists everything.
        globs: Vec<String>,
        /// Glob patterns to exclude (repeatable). Bare names like `vendor` exclude that
        /// directory at any depth; use `vendor/**` for explicit globbing.
        #[arg(long, value_name = "PATTERN")]
        exclude: Vec<String>,
        /// Maximum number of files to return.
        #[arg(long, default_value_t = DEFAULT_FILES_LIMIT)]
        limit: usize,
    },
    /// Summarize languages present in the repo (file count + line count per language).
    ///
    /// Useful for "is this a Rust project? what other languages?" orientation in one call.
    Langs,
    /// Inspect or manage the on-disk parse cache.
    ///
    /// The cache lives at $HITAGI_CACHE_DIR / $XDG_CACHE_HOME/hitagi /
    /// $HOME/.cache/hitagi (in that resolution order), keyed by canonical repo
    /// root. With no subcommand, prints `status`. Set HITAGI_NO_CACHE=1 in the
    /// environment to bypass the cache for any command.
    Cache {
        #[command(subcommand)]
        action: Option<CacheAction>,
    },
    /// Install the global hitagi prompt for an agent.
    ///
    /// Writes a small managed block to the agent's user-global instruction file:
    /// `~/.claude/CLAUDE.md` for Claude, `$CODEX_HOME/AGENTS.md` or
    /// `~/.codex/AGENTS.md` for Codex. If Codex has a non-empty
    /// `AGENTS.override.md`, installs there because it shadows `AGENTS.md`.
    Install {
        /// Agent to configure.
        #[arg(value_enum)]
        agent: AgentKind,
    },
    /// Remove the global hitagi prompt for an agent.
    ///
    /// Removes only hitagi's managed block, preserving surrounding user content.
    /// Codex uninstall checks both `AGENTS.md` and `AGENTS.override.md`.
    Uninstall {
        /// Agent to configure.
        #[arg(value_enum)]
        agent: AgentKind,
    },
    /// Show uncommitted changes (working tree vs HEAD by default).
    ///
    /// With no PATH, prints a one-entry-per-file overview (status, ±line counts,
    /// staged/unstaged flags, untracked files). With PATHS, prints structured
    /// hunks annotated by enclosing symbol; pass --summary for compact commit
    /// review, or --raw for unified diff text. Untracked files are drillable as
    /// synthetic additions.
    Diff {
        /// Optional file paths. Repo-relative; suffix resolution operates against
        /// the diff's own file list (so `Button.tsx` resolves like `outline`,
        /// and deleted files resolve too). Omit to print the overview.
        paths: Vec<String>,
        /// Narrow drilldown to hunks overlapping one symbol (qualname or unique
        /// leaf). Requires exactly one PATH. Mutually exclusive with --raw.
        #[arg(long, value_name = "QUALNAME", conflicts_with = "raw")]
        symbol: Option<String>,
        /// Drilldown only: emit raw unified diff text instead of structured
        /// hunks. Requires one or more PATHS.
        #[arg(long)]
        raw: bool,
        /// Summary mode: compact per-file output for commit review. With
        /// --symbols, includes touched symbol names instead of hunk bodies.
        #[arg(long)]
        summary: bool,
        /// Commit-review preset: compact summary with symbols and grouped text output.
        #[arg(long)]
        commit: bool,
        /// Summary only: include touched symbols per file.
        #[arg(long)]
        symbols: bool,
        /// Path-only output: one changed repo-relative path per line in text mode.
        #[arg(long = "paths")]
        diff_paths: bool,
        /// Alias for --paths.
        #[arg(long = "names-only")]
        names_only: bool,
        /// Structured drilldown body detail.
        #[arg(long, value_enum, default_value_t = CliDiffBodyMode::Full)]
        body: CliDiffBodyMode,
        /// Structured drilldown only: add the first changed line to each hunk header.
        #[arg(long)]
        snippet: bool,
        /// Show only staged changes (index vs the base ref).
        #[arg(long, conflicts_with_all = ["unstaged", "untracked"])]
        staged: bool,
        /// Show only unstaged changes (working tree vs index).
        #[arg(long, conflicts_with = "untracked")]
        unstaged: bool,
        /// Show only untracked files.
        #[arg(long)]
        untracked: bool,
        /// Compare against this ref instead of HEAD (e.g. `--against main`).
        #[arg(long, value_name = "REF", default_value = "HEAD")]
        against: String,
        /// Glob patterns to exclude files in the overview (repeatable). Bare
        /// names like `vendor` exclude that directory at any depth.
        #[arg(long, value_name = "PATTERN")]
        exclude: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum CliSearchMode {
    Hybrid,
    Bm25,
    Semantic,
}

impl From<CliSearchMode> for SearchModeArg {
    fn from(value: CliSearchMode) -> Self {
        match value {
            CliSearchMode::Hybrid => SearchModeArg::Hybrid,
            CliSearchMode::Bm25 => SearchModeArg::Bm25,
            CliSearchMode::Semantic => SearchModeArg::Semantic,
        }
    }
}

#[derive(Subcommand)]
enum IndexAction {
    /// Print indexed file/chunk counts, language breakdown, sparse/dense
    /// presence, and model fingerprint. Default action when no subcommand.
    Status,
    /// Force a rebuild of the search index. Useful after a model swap or
    /// to warm the cache before a session.
    Build {
        #[arg(short = 'm', long, value_enum, default_value_t = CliSearchMode::Hybrid)]
        mode: CliSearchMode,
        #[arg(long)]
        hashing: bool,
        #[arg(long = "no-download")]
        no_download: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long, value_name = "MODEL")]
        model: Option<String>,
    },
    /// Drop the search-index portion of the SQLite cache. The parse cache
    /// (used by outline/symbol/find) is left intact. Use `hitagi cache
    /// clear` to wipe both.
    Clean,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum CliDiffBodyMode {
    Full,
    ChangedLines,
    AddedOnly,
    None,
}

impl From<CliDiffBodyMode> for DiffBodyMode {
    fn from(value: CliDiffBodyMode) -> Self {
        match value {
            CliDiffBodyMode::Full => DiffBodyMode::Full,
            CliDiffBodyMode::ChangedLines => DiffBodyMode::ChangedLines,
            CliDiffBodyMode::AddedOnly => DiffBodyMode::AddedOnly,
            CliDiffBodyMode::None => DiffBodyMode::None,
        }
    }
}

#[derive(Subcommand)]
enum CacheAction {
    /// Print cache info: path, file size, entry count, language breakdown,
    /// version, whether the stored cache matches the current binary.
    Status,
    /// Print just the resolved cache directory path for this repo.
    Path,
    /// Delete the cache for the current repo. Pass `--all` to delete the
    /// entire hitagi cache root (every repo). Cache contents are fully
    /// regenerable ~ next find/search/outline rebuilds them.
    Clear {
        /// Delete every repo's cache, not just this one's.
        #[arg(long)]
        all: bool,
    },
}

pub fn run() -> AppResult<()> {
    let cli = Cli::parse();
    let mode = if cli.json {
        OutputMode::Json
    } else {
        OutputMode::Text
    };
    let command = cli.command;

    match command {
        Commands::Install { agent } => {
            let response = agent_prompt::install(agent)?;
            output::print_agent_prompt(&response, mode)
        }
        Commands::Uninstall { agent } => {
            let response = agent_prompt::uninstall(agent)?;
            output::print_agent_prompt(&response, mode)
        }
        command => {
            let repo_root = resolve_repo_root(cli.repo)?;
            let repo = RepoRoot::new(repo_root);

            match command {
                Commands::Outline {
                    path,
                    bytes,
                    kind,
                    depth,
                } => {
                    let opts = OutlineOptions {
                        bytes,
                        kinds: kind,
                        depth,
                    };
                    let response = commands::outline(&repo, &path, opts)?;
                    output::print_outline(&path, &response, mode)
                }
                Commands::Symbol {
                    path,
                    qualname,
                    bytes,
                } => {
                    let opts = SymbolOptions { bytes };
                    let response = commands::symbol(&repo, &path, &qualname, opts)?;
                    output::print_symbol(&path, &response, mode)
                }
                Commands::Search {
                    query,
                    paths,
                    limit,
                    mode: search_mode,
                    languages,
                    exclude,
                    alpha,
                    snippet,
                    hashing,
                    no_download,
                    offline,
                    model,
                } => {
                    let opts = SearchOptions {
                        paths,
                        excludes: exclude,
                        limit,
                        mode: search_mode.into(),
                        languages,
                        alpha,
                        snippet,
                        hashing,
                        no_download,
                        offline,
                        model,
                    };
                    let response = commands::search(&repo, &query, opts)?;
                    output::print_search(&response, mode)
                }
                Commands::FindRelated {
                    path,
                    line,
                    limit,
                    hashing,
                    no_download,
                    offline,
                    model,
                } => {
                    let opts = FindRelatedOptions {
                        limit,
                        hashing,
                        no_download,
                        offline,
                        model,
                    };
                    let response = commands::find_related(&repo, &path, line, opts)?;
                    output::print_find_related(&response, mode)
                }
                Commands::Index { action } => match action.unwrap_or(IndexAction::Status) {
                    IndexAction::Status => {
                        let response = commands::index_status(&repo);
                        output::print_index_status(&response, mode)
                    }
                    IndexAction::Build {
                        mode: build_mode,
                        hashing,
                        no_download,
                        offline,
                        model,
                    } => {
                        let opts = IndexBuildOptions {
                            mode: build_mode.into(),
                            hashing,
                            no_download,
                            offline,
                            model,
                        };
                        let response = commands::index_build(&repo, opts)?;
                        output::print_index_build(&response, mode)
                    }
                    IndexAction::Clean => {
                        let response = commands::index_clean(&repo)?;
                        output::print_index_clean(&response, mode)
                    }
                },
                Commands::Read {
                    path,
                    lines,
                    summary,
                } => {
                    if summary && lines.is_some() {
                        return Err(AppError::bad_request(
                            "--summary and --lines cannot be combined",
                        ));
                    }
                    let opts = ReadOptions {
                        lines: lines.as_deref().map(parse_lines).transpose()?,
                        summary,
                    };
                    if opts.summary {
                        let response = commands::read_summary(&repo, &path)?;
                        output::print_read_summary(&path, &response, mode)
                    } else {
                        let response = commands::read_file(&repo, &path, opts)?;
                        output::print_read(&path, &response, mode)
                    }
                }
                Commands::Find {
                    query,
                    paths,
                    kind,
                    limit,
                    bytes,
                    snippet,
                    terse,
                    per_file,
                    exclude,
                } => {
                    let opts = FindOptions {
                        paths,
                        excludes: exclude,
                        kinds: kind,
                        limit,
                        bytes,
                        snippet,
                        terse,
                        per_file,
                    };
                    let response = commands::find(&repo, &query, opts)?;
                    output::print_find(&query, &response, mode)
                }
                Commands::Files {
                    globs,
                    exclude,
                    limit,
                } => {
                    let opts = FilesOptions {
                        globs,
                        excludes: exclude,
                        limit,
                    };
                    let response = commands::files(&repo, opts)?;
                    output::print_files(&response, mode)
                }
                Commands::Langs => {
                    let response = commands::langs(&repo)?;
                    output::print_langs(&response, mode)
                }
                Commands::Cache { action } => match action.unwrap_or(CacheAction::Status) {
                    CacheAction::Status => {
                        let response = commands::cache_status(&repo);
                        output::print_cache_status(&response, mode)
                    }
                    CacheAction::Path => {
                        let response = commands::cache_path(&repo);
                        output::print_cache_path(&response, mode)
                    }
                    CacheAction::Clear { all } => {
                        let response = commands::cache_clear(&repo, all)?;
                        output::print_cache_clear(&response, mode)
                    }
                },
                Commands::Diff {
                    paths,
                    symbol,
                    raw,
                    summary,
                    commit,
                    symbols,
                    diff_paths,
                    names_only,
                    body,
                    snippet,
                    staged,
                    unstaged,
                    untracked,
                    against,
                    exclude,
                } => {
                    let scope = if staged {
                        DiffScope::Staged
                    } else if unstaged {
                        DiffScope::Unstaged
                    } else if untracked {
                        DiffScope::Untracked
                    } else {
                        DiffScope::All
                    };
                    let opts = DiffOptions {
                        scope,
                        against,
                        excludes: exclude,
                    };
                    let body = DiffBodyMode::from(body);
                    let paths_only = diff_paths || names_only;
                    if paths_only {
                        if summary {
                            return Err(AppError::bad_request(
                                "--paths and --summary cannot be combined",
                            ));
                        }
                        if commit {
                            return Err(AppError::bad_request(
                                "--paths and --commit cannot be combined",
                            ));
                        }
                        if raw {
                            return Err(AppError::bad_request(
                                "--paths and --raw cannot be combined",
                            ));
                        }
                        if symbol.is_some() {
                            return Err(AppError::bad_request(
                                "--paths and --symbol cannot be combined",
                            ));
                        }
                        if symbols {
                            return Err(AppError::bad_request(
                                "--paths and --symbols cannot be combined",
                            ));
                        }
                        if body != DiffBodyMode::Full {
                            return Err(AppError::bad_request(
                                "--paths and --body cannot be combined",
                            ));
                        }
                        if snippet {
                            return Err(AppError::bad_request(
                                "--paths and --snippet cannot be combined",
                            ));
                        }
                        let response = commands::diff_paths(&repo, &paths, opts)?;
                        return output::print_diff_paths(&response, mode);
                    }
                    if commit {
                        if summary {
                            return Err(AppError::bad_request(
                                "--commit and --summary cannot be combined",
                            ));
                        }
                        if raw {
                            return Err(AppError::bad_request(
                                "--commit and --raw cannot be combined",
                            ));
                        }
                        if symbol.is_some() {
                            return Err(AppError::bad_request(
                                "--commit and --symbol cannot be combined",
                            ));
                        }
                        if body != DiffBodyMode::Full {
                            return Err(AppError::bad_request(
                                "--commit and --body cannot be combined",
                            ));
                        }
                        if snippet {
                            return Err(AppError::bad_request(
                                "--commit and --snippet cannot be combined",
                            ));
                        }
                        let response = commands::diff_summary(
                            &repo,
                            &paths,
                            opts,
                            DiffSummaryOptions {
                                symbols: true,
                                commit: true,
                                group_by_state: true,
                            },
                        )?;
                        return output::print_diff_summary(&response, mode);
                    }
                    if symbols && !summary {
                        return Err(AppError::bad_request(
                            "--symbols requires --summary or --commit",
                        ));
                    }
                    if summary {
                        if raw {
                            return Err(AppError::bad_request(
                                "--summary and --raw cannot be combined",
                            ));
                        }
                        if symbol.is_some() {
                            return Err(AppError::bad_request(
                                "--summary and --symbol cannot be combined",
                            ));
                        }
                        if body != DiffBodyMode::Full {
                            return Err(AppError::bad_request(
                                "--summary and --body cannot be combined",
                            ));
                        }
                        if snippet {
                            return Err(AppError::bad_request(
                                "--summary and --snippet cannot be combined",
                            ));
                        }
                        let response = commands::diff_summary(
                            &repo,
                            &paths,
                            opts,
                            DiffSummaryOptions {
                                symbols,
                                commit: false,
                                group_by_state: false,
                            },
                        )?;
                        return output::print_diff_summary(&response, mode);
                    }
                    let no_drill_flags =
                        !raw && symbol.is_none() && body == DiffBodyMode::Full && !snippet;
                    if paths.is_empty() {
                        if raw {
                            return Err(AppError::bad_request("--raw requires PATH"));
                        }
                        if symbol.is_some() {
                            return Err(AppError::bad_request("--symbol requires PATH"));
                        }
                        if body != DiffBodyMode::Full {
                            return Err(AppError::bad_request("--body requires PATH"));
                        }
                        if snippet {
                            return Err(AppError::bad_request("--snippet requires PATH"));
                        }
                        let response = commands::diff_overview(&repo, opts)?;
                        output::print_diff_overview(&response, mode)
                    } else if no_drill_flags
                        && commands::diff_paths_are_all_directories(&repo, &paths, opts.clone())?
                    {
                        let response = commands::diff_summary(
                            &repo,
                            &paths,
                            opts,
                            DiffSummaryOptions {
                                symbols: false,
                                commit: false,
                                group_by_state: false,
                            },
                        )?;
                        output::print_diff_summary(&response, mode)
                    } else if paths.len() == 1 {
                        let drill = DiffFileOptions {
                            symbol,
                            raw,
                            body,
                            snippet,
                        };
                        let response = commands::diff_file(&repo, &paths[0], opts, drill)?;
                        output::print_diff_file(&paths[0], &response, mode)
                    } else {
                        if symbol.is_some() {
                            return Err(AppError::bad_request(
                                "--symbol requires exactly one PATH",
                            ));
                        }
                        let drill = DiffFileOptions {
                            symbol: None,
                            raw,
                            body,
                            snippet,
                        };
                        let response = commands::diff_files(&repo, &paths, opts, drill)?;
                        output::print_diff_files(&response, mode)
                    }
                }
                Commands::Install { .. } | Commands::Uninstall { .. } => unreachable!(),
            }
        }
    }
}

fn resolve_repo_root(flag: Option<PathBuf>) -> AppResult<PathBuf> {
    let candidate = match flag {
        Some(path) => path,
        None => std::env::current_dir().map_err(|error| {
            AppError::bad_request(format!("failed to read current directory: {error}"))
        })?,
    };

    let canonical = std::fs::canonicalize(&candidate).map_err(|error| {
        AppError::bad_request(format!(
            "failed to open repo root {}: {error}",
            candidate.display()
        ))
    })?;

    if !canonical.is_dir() {
        return Err(AppError::bad_request(format!(
            "repo root is not a directory: {}",
            canonical.display()
        )));
    }

    Ok(canonical)
}

fn parse_lines(spec: &str) -> AppResult<(usize, usize)> {
    let (start, end) = spec.split_once('-').ok_or_else(|| {
        AppError::bad_request(format!("--lines must be in S-E format, got: {spec}"))
    })?;
    let start = start.trim().parse::<usize>().map_err(|_| {
        AppError::bad_request(format!("--lines start is not a positive integer: {start}"))
    })?;
    let end = end.trim().parse::<usize>().map_err(|_| {
        AppError::bad_request(format!("--lines end is not a positive integer: {end}"))
    })?;
    Ok((start, end))
}
