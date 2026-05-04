use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::{
    commands::{
        self, DiffFileOptions, DiffOptions, DiffScope, FilesOptions, FindOptions, OutlineOptions,
        ReadOptions, SearchOptions, SymbolOptions,
    },
    error::{AppError, AppResult},
    output::{self, OutputMode},
    repo::RepoRoot,
};

const DEFAULT_SEARCH_LIMIT: usize = 50;
const DEFAULT_FIND_LIMIT: usize = 50;
const DEFAULT_FILES_LIMIT: usize = 2000;

const LONG_ABOUT: &str = "\
hitagi is a local CLI for tree-sitter-backed structural code queries, built for LLM \
coding agents (Claude Code, Codex, etc.) to navigate a codebase token-efficiently. \
Every command parses on demand, prints concise text to stdout, and exits ~ no daemon, \
no network, no auth. Pass --json for machine-readable output.

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
  6. search <STR>          substring search with scope + match-line annotation
  7. read <FILE>           dump a file (use --lines to slice big files)
  8. diff [FILE]           review uncommitted changes (overview, then drill)

SUPPORTED LANGUAGES
  PARSEABLE (full outline / symbol / find): Rust (.rs), TypeScript (.ts), TSX \
(.tsx), Python (.py), Kotlin (.kt/.kts), Prisma (.prisma).
  RECOGNISED (named in `langs`, `search`-able as plaintext, but no symbol info): \
JSON, YAML, TOML, Markdown, SQL, HTML, CSS, shell, Dockerfile.
  Truly unknown extensions get bucketed as `plaintext` ~ still searchable, just \
unlabelled.\
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

  Search syntax
    Combine alternatives with \" OR \" (literal, space-padded): \"foo OR bar\" is two
    terms, \"fooORbar\" is one literal. Each result reads
      `scope(kind) @L<line>`   for matches inside a parsed symbol
      `@L<line>`               for matches outside any scope (imports, comments,
                               plaintext files)
    Pass extra positional [PATHS] to scope the walk to subtrees.

  Snippets
    `--snippet` on `search` appends ` :: <matched line>` to each entry.
    `--snippet` on `find` adds the symbol's first-line signature.
    Both save a follow-up `read` when you only needed inline context.

  Limits and truncation
    Default --limit is 50 for search/find, 500 for files. When the cap is reached
    the response carries `\"truncated\": true`. Bump --limit when sweeping; reduce
    it for noisy queries.

  Fair sampling on full-repo sweeps
    `find` and `search` walk top-level subdirs round-robin (one file per bucket
    in turn) when no positional [PATHS] are given, so a `--limit` truncation
    produces a fair sample across the repo instead of exhausting the budget on
    whichever top-level dir comes first alphabetically. Pass [PATHS] to opt out
    and walk in user-supplied order. When matches still don't reach a subtree,
    `unsampled_dirs` lists what was skipped.

  Excluding noise (--exclude)
    `search`, `find`, and `files` accept --exclude PATTERN (repeatable). Bare names
    like `--exclude vendor` skip that directory at any depth; full globs like
    `--exclude \"vendor/**\"` work too. Typical: --exclude vendor --exclude target
    --exclude node_modules. Cuts grammar-source / build-artifact noise from sweeps.

  --kind filter
    Case-insensitive (function, Function, FUNCTION all match). When --kind matches
    nothing, the response includes `available_kinds: [...]` so you know what was
    actually present ~ no need for a second probe call.

  outline --depth N
    Limits nesting depth: --depth 1 keeps top-level shapes only; --depth 2 also
    keeps one level of nesting (e.g. methods inside a class, variants inside an
    enum). Depth is counted from dots in the qualname. Use it on big files where
    you only need orientation.

  find --terse
    Compact output mode. `matches` becomes a list of strings like
    `path:line qualname(kind)` instead of structured objects ~ ~3x smaller for
    sweep queries. With --snippet the line continues with ` :: <signature>`.

  find --per-file N
    Cap matches per file at N (default 0 = no cap). Useful when one file has
    a class with many methods that all match the query and would otherwise
    eat the global --limit budget. Suppressed match counts are reported per-
    file via `more_in_file: { \"path\": <count>, ... }` (top-level on flat
    responses, inside the containing group on grouped responses). The cap
    counts toward --limit ~ it's a diversity control, not a bypass.

  Per-prefix grouping (find / search response shape)
    When matches all share a common path prefix, the response stays flat:
    top-level `prefix` plus `matches` (find) or `results` (search) with the
    prefix stripped. When matches span multiple top-level dirs and there's
    no shared prefix, the response switches to grouped form: a `groups: [...]`
    array where each group carries its own `prefix` plus its own `matches`/
    `results` (and `more_in_file` for find). Cuts repeated long monorepo
    paths out of the output. Top-level `matches`/`results` is `[]`/`{}` in
    grouped form. Round-robin sampling and grouping work together: the walk
    visits diverse subtrees, and grouping hoists each subtree's prefix.

  langs and parseable
    `langs` summarises file count + line count per detected language and includes
    `parseable: bool`. Languages with `parseable: true` (Rust/TS/TSX/Python/Kotlin/
    Prisma) work with outline/symbol/find. Recognised non-parseable ones (json,
    markdown, sql, css, shell, dockerfile, ...) only show up in `langs`/`search`/
    `read`.

  When `find` returns nothing
    `find` only matches qualnames in PARSEABLE files (.md/.txt/.toml etc. are
    skipped). For raw substring search across all file types, use `search`. The
    `searched_files` field tells you how many files actually got parsed; if it's 0
    the response includes a `note` explaining why.

  Parse cache
    find / outline / search / symbol persist parsed symbols at
    $HITAGI_CACHE_DIR / $XDG_CACHE_HOME/hitagi / $HOME/.cache/hitagi, keyed by
    (path, mtime, size, language). Warm runs skip the parse step (and skip the
    file read entirely for `find` without --snippet and `outline`), turning a
    multi-second cold sweep into ~100ms. Set HITAGI_NO_CACHE=1 to bypass for one
    invocation. Use `hitagi cache status` to inspect, `hitagi cache clear` to
    drop the current repo's cache, `hitagi cache clear --all` to nuke all of
    them.

  Uncommitted changes (`diff`)
    `diff` (no PATH) prints a per-file overview ~ status code (M/A/D/R/C/?),
    ±line counts, staged/unstaged flags. Untracked files appear as `?` with no
    counts (drilldown not supported; use `read` for content). `diff <PATH>`
    prints structured hunks with the enclosing symbol annotated; pass --raw for
    the unified diff text instead. `--symbol Q` filters hunks to those
    overlapping one symbol (uses the same qualname/leaf semantics as `symbol`).
    `--staged` / `--unstaged` narrow the scope; default combines both. Use
    `--against REF` to compare against something other than HEAD (e.g. main).
    Deleted files get their HEAD-side blob parsed in-memory so `symbol`
    annotations still appear. The structured-hunks response degrades to ranges
    + symbols (no `body`) when one file's diff exceeds the size cap; a top-
    level `note` explains. Subprocess overhead is small (a few `git diff`
    invocations).

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
  Where's this string used?       hitagi search \"X\" --snippet
  Where's it used (specific dir)? hitagi search \"X\" src/auth
  Sweep without vendor noise      hitagi search \"X\" --exclude vendor --exclude target
  Read a slice of a big file      hitagi read FILE --lines 1400-1510
  List all Rust + TOML files      hitagi files \"**/*.rs\" \"**/*.toml\"
  Find Auth-related classes       hitagi find Auth --kind class,struct --snippet
  Find inside one subtree         hitagi find Network --kind struct src/nnue
  Cheap top-level orientation     hitagi outline FILE --depth 1
  Cheap sweep across the repo     hitagi find X --terse --limit 200
  Diverse sweep (cap hot files)   hitagi find X --terse --per-file 3
  What's uncommitted?             hitagi diff
  Hunks for one file?             hitagi diff src/foo.rs
  Diff for one symbol?            hitagi diff src/foo.rs --symbol Foo.bar
  Just staged changes             hitagi diff --staged
  Compare against main            hitagi diff --against main

ANTI-PATTERNS (token waste)

  hitagi read big_file.rs                  # ~5K-line files cost a lot of tokens.
                                           # Use `outline` then `symbol`, or
                                           # `read --lines S-E` for slices.
  hitagi search \"the\"                      # tighten queries; pass [PATHS] to scope.
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
  search    {\"prefix\":\"src/\",\"results\":{\"file.rs\":[\"scope(kind) @L<n>\"]},
            \"truncated\":bool}
            grouped (when matches span top-levels with no shared prefix):
            {\"results\":{},\"groups\":[{\"prefix\":\"a/\",\"results\":{\"f.rs\":
            [\"...\"]}},{\"prefix\":\"b/\",\"results\":{...}}],\"truncated\":bool}
  read      {\"language\":\"rust\",\"content\":\"...\",\"lines\":[s,e],
            \"total_lines\":N}    (lines/total_lines only when --lines is passed)
  find      {\"prefix\":\"src/\"?,\"matches\":[{\"path\":\"...\",\"kind\":\"...\",
            \"name\":\"...\",\"qualname\":\"...\",\"lines\":[s,e]}],
            \"more_in_file\":{\"path\":N,...}?,\"truncated\":bool,
            \"searched_files\":N,\"available_kinds\":[...]?,\"note\":\"...\"?}
            grouped (when matches span top-levels with no shared prefix):
            {\"matches\":[],\"groups\":[{\"prefix\":\"a/\",\"matches\":[...],
            \"more_in_file\":{...}?},...],\"truncated\":bool,
            \"searched_files\":N,...}
  files     {\"files\":[\"a\",\"b\",...],\"truncated\":bool,\"note\":\"...\"?}
  langs     {\"languages\":[{\"language\":\"rust\",\"files\":N,\"lines\":N,
            \"parseable\":bool},...]}
  diff      overview: {\"prefix\":\"...\"?,\"files\":[{\"path\":\"...\",
            \"status\":\"M|A|D|R|C|?\",\"old_path\":\"...\"?,\"added\":N?,
            \"removed\":N?,\"staged\":bool?,\"unstaged\":bool?,\"binary\":
            bool?,\"note\":\"...\"?},...],\"against\":\"...\"?,
            \"scope\":\"staged|unstaged\"?,\"clean\":bool?,\"note\":\"...\"?}
            drilldown: {\"path\":\"...\",\"status\":\"...\",\"old_path\":\"...\"?,
            \"added\":N?,\"removed\":N?,\"language\":\"...\"?,\"hunks\":
            [{\"old_lines\":[s,e],\"new_lines\":[s,e],\"added\":N,\"removed\":N,
            \"symbol\":\"...\"?,\"kind\":\"...\"?,\"spans\":[\"...\"]?,\"body\":
            \"...\"?}],\"raw\":\"...\"?,\"binary\":bool?,\"note\":\"...\"?}

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
    /// Substring search across the repo, grouped by enclosing scope.
    ///
    /// QUERY is a literal substring. Combine alternatives with ` OR ` (literal,
    /// surrounded by spaces), e.g. `"foo OR bar"`. Each result is annotated with the
    /// enclosing symbol scope (when known) and the actual match line, e.g.
    /// `parse_source(function) @L23` ~ unscoped matches show only `@L23`.
    Search {
        /// Substring query. Use ` OR ` (space-padded) to combine alternatives.
        query: String,
        /// Optional path prefixes to scope the search.
        paths: Vec<String>,
        /// Maximum total matches to return. Response includes `truncated: true` when hit.
        #[arg(long, default_value_t = DEFAULT_SEARCH_LIMIT)]
        limit: usize,
        /// Append the matched line as a snippet (` :: <line>`) for inline context.
        #[arg(long)]
        snippet: bool,
        /// Glob patterns to exclude (repeatable). Bare names like `vendor` exclude that
        /// directory at any depth; use `vendor/**` for explicit globbing.
        #[arg(long, value_name = "PATTERN")]
        exclude: Vec<String>,
    },
    /// Read a file's contents.
    Read {
        /// File path relative to the repo root (or a unique repo-internal suffix).
        path: String,
        /// Slice to a 1-indexed inclusive line range, e.g. `--lines 100-200`.
        #[arg(long, value_name = "S-E")]
        lines: Option<String>,
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
        /// strings instead of structured objects. ~3x smaller for sweep queries.
        #[arg(long)]
        terse: bool,
        /// Cap matches per file at N (0 = no cap, default). When the cap is hit,
        /// the count of suppressed matches per file is reported in `more_in_file`.
        /// Counted toward --limit ~ this is a diversity control, not a bypass.
        #[arg(long, value_name = "N", default_value_t = 0)]
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
    /// Show uncommitted changes (working tree vs HEAD by default).
    ///
    /// With no PATH, prints a one-entry-per-file overview (status, ±line counts,
    /// staged/unstaged flags, untracked files). With PATH, prints structured
    /// hunks annotated by enclosing symbol; pass --raw for the unified diff text
    /// instead. Untracked files appear in the overview but cannot be drilled
    /// into ~ use `read` for their content.
    Diff {
        /// Optional file path. Repo-relative; suffix resolution operates against
        /// the diff's own file list (so `Button.tsx` resolves like `outline`,
        /// and deleted files resolve too). Omit to print the overview.
        path: Option<String>,
        /// Narrow drilldown to hunks overlapping one symbol (qualname or unique
        /// leaf). Requires PATH. Mutually exclusive with --raw.
        #[arg(long, value_name = "QUALNAME", conflicts_with = "raw")]
        symbol: Option<String>,
        /// Drilldown only: emit raw unified diff text instead of structured
        /// hunks. Requires PATH.
        #[arg(long)]
        raw: bool,
        /// Show only staged changes (index vs the base ref).
        #[arg(long, conflicts_with = "unstaged")]
        staged: bool,
        /// Show only unstaged changes (working tree vs index).
        #[arg(long)]
        unstaged: bool,
        /// Compare against this ref instead of HEAD (e.g. `--against main`).
        #[arg(long, value_name = "REF", default_value = "HEAD")]
        against: String,
        /// Glob patterns to exclude files in the overview (repeatable). Bare
        /// names like `vendor` exclude that directory at any depth.
        #[arg(long, value_name = "PATTERN")]
        exclude: Vec<String>,
    },
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
    let repo_root = resolve_repo_root(cli.repo)?;
    let repo = RepoRoot::new(repo_root);

    match cli.command {
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
            snippet,
            exclude,
        } => {
            let opts = SearchOptions {
                paths,
                excludes: exclude,
                limit,
                snippet,
            };
            let response = commands::search(&repo, &query, opts)?;
            output::print_search(&query, &response, mode)
        }
        Commands::Read { path, lines } => {
            let opts = ReadOptions {
                lines: lines.as_deref().map(parse_lines).transpose()?,
            };
            let response = commands::read_file(&repo, &path, opts)?;
            output::print_read(&path, &response, mode)
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
            path,
            symbol,
            raw,
            staged,
            unstaged,
            against,
            exclude,
        } => {
            let scope = if staged {
                DiffScope::Staged
            } else if unstaged {
                DiffScope::Unstaged
            } else {
                DiffScope::All
            };
            let opts = DiffOptions {
                scope,
                against,
                excludes: exclude,
            };
            match path {
                None => {
                    let response = commands::diff_overview(&repo, opts)?;
                    output::print_diff_overview(&response, mode)
                }
                Some(p) => {
                    let drill = DiffFileOptions { symbol, raw };
                    let response = commands::diff_file(&repo, &p, opts, drill)?;
                    output::print_diff_file(&p, &response, mode)
                }
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
