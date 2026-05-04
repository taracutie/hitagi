use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ffi::OsString,
    fs::Metadata,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Transaction};

use crate::{
    lang::{Language, LineStats},
    models::SymbolInfo,
};

// Bumping the crate version invalidates all caches ~ cheapest proxy for
// "visitor logic in parser.rs may have changed shape". Schema bumps just
// change the v<N> prefix. Mismatch on either falls back to an empty cache.
//
// v3 moved from a repo-wide bincode blob to a SQLite row store, so one-file
// queries can fetch and update one cache entry instead of deserializing and
// rewriting every cached file in the repo.
// v4 split line_count into line_total / line_blank / line_comment so `langs`
// can break out cloc-style code/comment/blank columns.
const CACHE_VERSION_KEY: &str = concat!("v4-", env!("CARGO_PKG_VERSION"));
const CACHE_FILE_NAME: &str = "index.v4.sqlite";

#[derive(Clone)]
struct FileEntry {
    mtime_secs: i64,
    mtime_nanos: u32,
    size: u64,
    language: Language,
    /// Line breakdown of the file's content as last seen. Both parsed entries
    /// and lang-only stamps populate this so `langs` warm runs are O(stat).
    line_stats: Option<LineStats>,
    /// True when `symbols` came from `parse_source`. False when this entry
    /// was stamped by `langs` (line_count only); the symbols vec is empty
    /// in that case and the next outline/find/symbol call must re-parse.
    parsed_for_symbols: bool,
    symbols: Vec<SymbolInfo>,
}

pub struct ParseCache {
    cache_dir: PathBuf,
    cache_file: PathBuf,
    repo_root: String,
    conn: Option<Connection>,
    checked_existing: bool,
    reset_on_write: bool,
    pending: HashMap<String, FileEntry>,
    /// On-disk rows hoisted into memory by `ensure_loaded`. Populated lazily
    /// on the first `lookup_entry` call so a one-shot bulk SELECT replaces
    /// the per-file SELECTs that used to dominate warm-cache walks. Stays
    /// empty when the cache file doesn't exist or is unreadable ~ subsequent
    /// lookups simply miss.
    loaded: HashMap<String, FileEntry>,
    /// Sticky flag: `true` once `ensure_loaded` has run, regardless of
    /// outcome. Stops every subsequent lookup from re-attempting the bulk
    /// read.
    loaded_done: bool,
    seen: HashSet<String>,
    enabled: bool,
}

impl ParseCache {
    /// Open the cache for `repo_root` (canonical). Always succeeds: any failure
    /// (missing file, decode error, version mismatch, repo_root mismatch, disabled
    /// via env) yields an empty in-memory cache that can be populated and saved.
    pub fn open(repo_root: &Path) -> Self {
        let repo_root_str = repo_root.to_string_lossy().into_owned();
        let enabled = !env_disabled();
        let cache_dir = resolve_cache_dir(&repo_root_str);
        Self::open_inner(cache_dir, repo_root_str, enabled)
    }

    /// Open the cache at an explicit directory, bypassing env-var resolution.
    /// Useful for unit tests that need filesystem isolation without touching
    /// the shared process environment.
    #[cfg(test)]
    pub fn open_at(cache_dir: PathBuf, repo_root: &Path) -> Self {
        let repo_root_str = repo_root.to_string_lossy().into_owned();
        let dir = cache_dir.join(repo_hash(&repo_root_str));
        Self::open_inner(Some(dir), repo_root_str, true)
    }

    /// Construct an always-disabled cache. Useful for unit tests that exercise
    /// the parse path without touching any filesystem state.
    #[cfg(test)]
    pub fn disabled() -> Self {
        Self::empty(PathBuf::new(), String::new(), false)
    }

    fn open_inner(cache_dir: Option<PathBuf>, repo_root: String, enabled: bool) -> Self {
        let cache_dir = match cache_dir {
            Some(dir) => dir,
            None => return Self::empty(PathBuf::new(), repo_root, false),
        };
        if !enabled {
            return Self::empty(cache_dir, repo_root, false);
        }
        let cache_file = cache_dir.join(CACHE_FILE_NAME);
        Self {
            cache_dir,
            cache_file,
            repo_root,
            conn: None,
            checked_existing: false,
            reset_on_write: false,
            pending: HashMap::new(),
            loaded: HashMap::new(),
            loaded_done: false,
            seen: HashSet::new(),
            enabled: true,
        }
    }

    fn empty(cache_dir: PathBuf, repo_root: String, enabled: bool) -> Self {
        let cache_file = cache_dir.join(CACHE_FILE_NAME);
        Self {
            cache_dir,
            cache_file,
            repo_root,
            conn: None,
            checked_existing: false,
            reset_on_write: false,
            pending: HashMap::new(),
            loaded: HashMap::new(),
            loaded_done: false,
            seen: HashSet::new(),
            enabled,
        }
    }

    /// Returns cached symbols when (mtime, size, language) match AND the
    /// stored entry was actually parsed (not a langs stamp). Records the
    /// path as seen either way ~ a "negative" lookup (miss) still counts as a
    /// path we walked, so prune doesn't drop entries we just couldn't reuse.
    pub fn lookup(
        &mut self,
        rel_path: &str,
        metadata: &Metadata,
        language: Language,
    ) -> Option<Vec<SymbolInfo>> {
        let entry = self.lookup_entry(rel_path, metadata, language)?;
        if !entry.parsed_for_symbols {
            return None;
        }
        Some(entry.symbols)
    }

    /// Returns the cached line count when (mtime, size, language) match.
    /// Used by `langs` to skip re-reading every file's bytes on warm runs.
    /// Lang-only entries (symbols.is_empty()) populate this for non-parseable
    /// languages too.
    pub fn lookup_line_stats(
        &mut self,
        rel_path: &str,
        metadata: &Metadata,
        language: Language,
    ) -> Option<LineStats> {
        let entry = self.lookup_entry(rel_path, metadata, language)?;
        entry.line_stats
    }

    fn lookup_entry(
        &mut self,
        rel_path: &str,
        metadata: &Metadata,
        language: Language,
    ) -> Option<FileEntry> {
        if !self.enabled {
            return None;
        }
        self.seen.insert(rel_path.to_string());

        if let Some(entry) = self.pending.get(rel_path) {
            return entry_matches(entry, metadata, language).then(|| entry.clone());
        }

        // First call materializes every row from the on-disk cache into
        // `self.loaded` ~ subsequent lookups are HashMap O(1). Replaces the
        // per-file SELECT round-trip that used to dominate warm walks (~3600
        // SELECTs on a 4400-file repo).
        self.ensure_loaded();

        let entry = self.loaded.get(rel_path)?;
        entry_matches(entry, metadata, language).then(|| entry.clone())
    }

    /// Bulk-load every row from `files` into `self.loaded`. Idempotent: the
    /// `loaded_done` flag is set on the first call regardless of outcome, so
    /// a missing/corrupt cache does not retry on every lookup.
    fn ensure_loaded(&mut self) {
        if self.loaded_done {
            return;
        }
        self.loaded_done = true;

        let mut loaded: HashMap<String, FileEntry> = HashMap::new();
        let mut had_error = false;

        {
            let Some(conn) = self.ensure_read_conn() else {
                return;
            };
            match conn.prepare(
                "SELECT rel_path, mtime_secs, mtime_nanos, size, language,
                        line_total, line_blank, line_comment, parsed_for_symbols, symbols_blob
                 FROM files",
            ) {
                Ok(mut stmt) => match stmt.query_map([], read_raw_entry) {
                    Ok(rows) => {
                        for row in rows {
                            match row {
                                Ok(raw) => {
                                    if let Some((rel_path, entry)) = decode_entry(raw) {
                                        loaded.insert(rel_path, entry);
                                    }
                                    // Coercion failures (unknown language label,
                                    // bincode mismatch) silently skip ~ same
                                    // semantics as the per-row loader's
                                    // `Ok(None)` branch.
                                }
                                Err(_) => had_error = true,
                            }
                        }
                    }
                    Err(_) => had_error = true,
                },
                Err(_) => had_error = true,
            }
        }

        self.loaded = loaded;
        if had_error {
            self.reset_on_write = true;
        }
    }

    /// Insert a fully-parsed entry (symbols + line_count). Used by the
    /// outline/find/symbol/search hot path; subsequent `lookup` calls return
    /// the symbols vec.
    pub fn insert(
        &mut self,
        rel_path: String,
        metadata: &Metadata,
        language: Language,
        line_stats: Option<LineStats>,
        symbols: Vec<SymbolInfo>,
    ) {
        self.insert_entry(rel_path, metadata, language, line_stats, symbols, true);
    }

    /// Insert a langs-only stamp: line_stats without parsed symbols. The
    /// next outline/find/symbol call for this path treats it as a cache
    /// miss and re-parses, replacing this entry with a full one.
    pub fn insert_lang_only(
        &mut self,
        rel_path: String,
        metadata: &Metadata,
        language: Language,
        line_stats: LineStats,
    ) {
        self.insert_entry(
            rel_path,
            metadata,
            language,
            Some(line_stats),
            Vec::new(),
            false,
        );
    }

    fn insert_entry(
        &mut self,
        rel_path: String,
        metadata: &Metadata,
        language: Language,
        line_stats: Option<LineStats>,
        symbols: Vec<SymbolInfo>,
        parsed_for_symbols: bool,
    ) {
        if !self.enabled {
            return;
        }
        let Some((secs, nanos)) = mtime_parts(metadata) else {
            return;
        };
        self.seen.insert(rel_path.clone());
        self.pending.insert(
            rel_path,
            FileEntry {
                mtime_secs: secs,
                mtime_nanos: nanos,
                size: metadata.len(),
                language,
                line_stats,
                parsed_for_symbols,
                symbols,
            },
        );
    }

    /// Persist pending entries to disk. When `prune_unseen` is true, drop entries
    /// not visited during this run ~ only safe when the walk covered the whole
    /// repo. Failures are silent: a stale or missing cache must never break a
    /// command.
    pub fn save(mut self, prune_unseen: bool) {
        if !self.enabled {
            return;
        }
        if self.pending.is_empty() && !prune_unseen {
            return;
        }
        if self.pending.is_empty()
            && prune_unseen
            && self.conn.is_none()
            && !self.cache_file.exists()
        {
            return;
        }
        if self.pending.is_empty() && prune_unseen && self.conn.is_none() {
            let _ = self.ensure_read_conn();
        }
        if self.pending.is_empty() && self.conn.is_none() {
            return;
        }

        let pending = std::mem::take(&mut self.pending);
        let seen = if prune_unseen {
            Some(std::mem::take(&mut self.seen))
        } else {
            None
        };
        let Some(conn) = self.ensure_write_conn() else {
            return;
        };

        if write_entries(conn, &pending, seen.as_ref()).is_ok() {
            return;
        }

        self.conn.take();
        let _ = std::fs::remove_file(&self.cache_file);
        self.reset_on_write = false;
        if let Some(conn) = self.ensure_write_conn() {
            let _ = write_entries(conn, &pending, seen.as_ref());
        }
    }

    /// Where the cache for `repo_root` would live, given current env vars.
    /// Returns None when no cache root could be resolved (e.g. no $HOME).
    pub fn cache_dir_for(repo_root: &Path) -> Option<PathBuf> {
        resolve_cache_dir(&repo_root.to_string_lossy())
    }

    /// Read everything we know about `repo_root`'s on-disk cache without
    /// modifying anything. Always succeeds: missing/corrupt files just yield
    /// an inspection where `exists` is false or the parsed fields are None.
    pub fn inspect(repo_root: &Path) -> CacheInspection {
        let repo_root_str = repo_root.to_string_lossy().into_owned();
        let cache_dir = resolve_cache_dir(&repo_root_str);
        let cache_file = cache_dir.as_ref().map(|d| d.join(CACHE_FILE_NAME));
        let disabled = env_disabled();

        let mut inspection = CacheInspection {
            enabled: !disabled && cache_dir.is_some(),
            disabled_via_env: disabled,
            current_version: CACHE_VERSION_KEY.to_string(),
            cache_dir,
            cache_file: cache_file.clone(),
            exists: false,
            size_bytes: 0,
            modified_unix_secs: None,
            stored_version: None,
            stored_repo_root: None,
            version_match: false,
            repo_root_match: false,
            entry_count: 0,
            languages: BTreeMap::new(),
        };

        let Some(file) = cache_file else {
            return inspection;
        };
        let Ok(meta) = std::fs::metadata(&file) else {
            return inspection;
        };
        inspection.exists = true;
        inspection.size_bytes = meta.len();
        inspection.modified_unix_secs = mtime_parts(&meta).map(|(s, _)| s);

        let Ok(conn) = Connection::open_with_flags(&file, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
            return inspection;
        };

        inspection.stored_version = meta_value(&conn, "version").ok().flatten();
        inspection.stored_repo_root = meta_value(&conn, "repo_root").ok().flatten();
        inspection.version_match = inspection
            .stored_version
            .as_deref()
            .map(|v| v == CACHE_VERSION_KEY)
            .unwrap_or(false);
        inspection.repo_root_match = inspection
            .stored_repo_root
            .as_deref()
            .map(|r| r == repo_root_str)
            .unwrap_or(false);

        if !(inspection.version_match && inspection.repo_root_match) {
            return inspection;
        }

        if let Ok(count) =
            conn.query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, i64>(0))
        {
            inspection.entry_count = usize::try_from(count).unwrap_or(0);
        }

        if let Ok(mut stmt) = conn.prepare("SELECT language, COUNT(*) FROM files GROUP BY language")
        {
            if let Ok(rows) = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            }) {
                for row in rows.flatten() {
                    if let Ok(files) = usize::try_from(row.1) {
                        inspection.languages.insert(row.0, files);
                    }
                }
            }
        }

        inspection
    }

    /// Remove the cache directory for `repo_root` (everything under
    /// `<base>/<repo-hash>/`). Returns the path that would be removed and
    /// whether it actually existed.
    pub fn clear(repo_root: &Path) -> std::io::Result<CacheClearOutcome> {
        let dir = match resolve_cache_dir(&repo_root.to_string_lossy()) {
            Some(d) => d,
            None => {
                return Ok(CacheClearOutcome {
                    path: PathBuf::new(),
                    existed: false,
                });
            }
        };
        let existed = dir.exists();
        if existed {
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(CacheClearOutcome { path: dir, existed })
    }

    /// Remove the entire hitagi cache root (all repos). Returns the path and
    /// the count of repo subdirs that were present before deletion.
    pub fn clear_all() -> std::io::Result<CacheClearAllOutcome> {
        let root = match cache_root() {
            Some(r) => r,
            None => {
                return Ok(CacheClearAllOutcome {
                    path: PathBuf::new(),
                    existed: false,
                    repos_removed: 0,
                });
            }
        };
        if !root.exists() {
            return Ok(CacheClearAllOutcome {
                path: root,
                existed: false,
                repos_removed: 0,
            });
        }
        let repos_removed = std::fs::read_dir(&root)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .count();
        std::fs::remove_dir_all(&root)?;
        Ok(CacheClearAllOutcome {
            path: root,
            existed: true,
            repos_removed,
        })
    }

    fn ensure_read_conn(&mut self) -> Option<&Connection> {
        if self.conn.is_some() {
            return self.conn.as_ref();
        }
        if self.checked_existing {
            return None;
        }
        self.checked_existing = true;
        if !self.cache_file.exists() {
            return None;
        }

        match Connection::open_with_flags(&self.cache_file, OpenFlags::SQLITE_OPEN_READ_WRITE) {
            Ok(conn) if validate_db(&conn, &self.repo_root) => {
                self.conn = Some(conn);
                self.conn.as_ref()
            }
            Ok(_) | Err(_) => {
                self.reset_on_write = true;
                None
            }
        }
    }

    fn ensure_write_conn(&mut self) -> Option<&mut Connection> {
        if self.reset_on_write {
            self.conn.take();
            let _ = std::fs::remove_file(&self.cache_file);
            self.reset_on_write = false;
        }

        if self.conn.is_none() {
            if std::fs::create_dir_all(&self.cache_dir).is_err() {
                return None;
            }
            let conn = Connection::open(&self.cache_file).ok()?;
            if init_db(&conn, &self.repo_root).is_err() {
                let _ = std::fs::remove_file(&self.cache_file);
                return None;
            }
            self.checked_existing = true;
            self.conn = Some(conn);
        }

        self.conn.as_mut()
    }
}

#[derive(Debug, Clone)]
pub struct CacheInspection {
    pub enabled: bool,
    pub disabled_via_env: bool,
    pub current_version: String,
    pub cache_dir: Option<PathBuf>,
    pub cache_file: Option<PathBuf>,
    pub exists: bool,
    pub size_bytes: u64,
    pub modified_unix_secs: Option<i64>,
    pub stored_version: Option<String>,
    pub stored_repo_root: Option<String>,
    pub version_match: bool,
    pub repo_root_match: bool,
    pub entry_count: usize,
    pub languages: BTreeMap<String, usize>,
}

#[derive(Debug, Clone)]
pub struct CacheClearOutcome {
    pub path: PathBuf,
    pub existed: bool,
}

#[derive(Debug, Clone)]
pub struct CacheClearAllOutcome {
    pub path: PathBuf,
    pub existed: bool,
    pub repos_removed: usize,
}

fn env_disabled() -> bool {
    matches!(std::env::var_os("HITAGI_NO_CACHE"), Some(value) if !value.is_empty() && value != "0")
}

fn valid_cache_root(value: OsString) -> Option<PathBuf> {
    let path = PathBuf::from(value);
    if path.as_os_str().is_empty() || !path.is_absolute() {
        None
    } else {
        Some(path)
    }
}

fn cache_root() -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os("HITAGI_CACHE_DIR") {
        return valid_cache_root(custom);
    }
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME").and_then(valid_cache_root) {
        return Some(xdg.join("hitagi"));
    }
    let home = std::env::var_os("HOME").and_then(valid_cache_root)?;
    Some(home.join(".cache").join("hitagi"))
}

fn resolve_cache_dir(repo_root: &str) -> Option<PathBuf> {
    cache_root().map(|base| base.join(repo_hash(repo_root)))
}

fn init_db(conn: &Connection, repo_root: &str) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS files (
            rel_path TEXT PRIMARY KEY,
            mtime_secs INTEGER NOT NULL,
            mtime_nanos INTEGER NOT NULL,
            size INTEGER NOT NULL,
            language TEXT NOT NULL,
            line_total INTEGER,
            line_blank INTEGER,
            line_comment INTEGER,
            parsed_for_symbols INTEGER NOT NULL,
            symbols_blob BLOB NOT NULL
        );
        ",
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('version', ?1)",
        params![CACHE_VERSION_KEY],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('repo_root', ?1)",
        params![repo_root],
    )?;
    Ok(())
}

fn validate_db(conn: &Connection, repo_root: &str) -> bool {
    let version = meta_value(conn, "version").ok().flatten();
    let stored_repo_root = meta_value(conn, "repo_root").ok().flatten();
    version.as_deref() == Some(CACHE_VERSION_KEY)
        && stored_repo_root.as_deref() == Some(repo_root)
        && files_table_is_readable(conn)
}

fn meta_value(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .optional()
}

fn files_table_is_readable(conn: &Connection) -> bool {
    conn.prepare(
        "SELECT rel_path, mtime_secs, mtime_nanos, size, language,
                line_total, line_blank, line_comment, parsed_for_symbols, symbols_blob
         FROM files LIMIT 0",
    )
    .is_ok()
}

struct RawEntry {
    rel_path: String,
    mtime_secs: i64,
    mtime_nanos: i64,
    size: i64,
    language: String,
    line_total: Option<i64>,
    line_blank: Option<i64>,
    line_comment: Option<i64>,
    parsed_for_symbols: i64,
    symbols_blob: Vec<u8>,
}

fn read_raw_entry(row: &rusqlite::Row) -> rusqlite::Result<RawEntry> {
    Ok(RawEntry {
        rel_path: row.get(0)?,
        mtime_secs: row.get(1)?,
        mtime_nanos: row.get(2)?,
        size: row.get(3)?,
        language: row.get(4)?,
        line_total: row.get(5)?,
        line_blank: row.get(6)?,
        line_comment: row.get(7)?,
        parsed_for_symbols: row.get(8)?,
        symbols_blob: row.get(9)?,
    })
}

/// Convert a raw SQLite row tuple into `(rel_path, FileEntry)`. Returns
/// `None` on any coercion failure (unknown language, bincode mismatch,
/// out-of-range integer) ~ the row is silently dropped, matching the
/// pre-prefetch per-row loader's `Ok(None)` semantics.
fn decode_entry(raw: RawEntry) -> Option<(String, FileEntry)> {
    let parsed_for_symbols = raw.parsed_for_symbols != 0;
    let symbols = if parsed_for_symbols {
        bincode::deserialize(&raw.symbols_blob).ok()?
    } else {
        Vec::new()
    };

    let mtime_nanos = u32::try_from(raw.mtime_nanos).ok()?;
    let size = u64::try_from(raw.size).ok()?;
    let language = language_from_str(&raw.language)?;
    let line_stats = raw.line_total.and_then(|t| {
        let total = u32::try_from(t).ok()?;
        let blank = u32::try_from(raw.line_blank.unwrap_or(0)).ok()?;
        let comment = u32::try_from(raw.line_comment.unwrap_or(0)).ok()?;
        Some(LineStats {
            total,
            blank,
            comment,
        })
    });

    Some((
        raw.rel_path,
        FileEntry {
            mtime_secs: raw.mtime_secs,
            mtime_nanos,
            size,
            language,
            line_stats,
            parsed_for_symbols,
            symbols,
        },
    ))
}

fn write_entries(
    conn: &mut Connection,
    pending: &HashMap<String, FileEntry>,
    seen: Option<&HashSet<String>>,
) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    for (rel_path, entry) in pending {
        upsert_entry(&tx, rel_path, entry)?;
    }
    if let Some(seen) = seen {
        prune_entries(&tx, seen)?;
    }
    tx.commit()
}

fn upsert_entry(tx: &Transaction<'_>, rel_path: &str, entry: &FileEntry) -> rusqlite::Result<()> {
    let symbols_blob = if entry.parsed_for_symbols {
        bincode::serialize(&entry.symbols)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
    } else {
        Vec::new()
    };
    let mtime_nanos = i64::from(entry.mtime_nanos);
    let size = i64::try_from(entry.size)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let line_total = entry.line_stats.map(|s| i64::from(s.total));
    let line_blank = entry.line_stats.map(|s| i64::from(s.blank));
    let line_comment = entry.line_stats.map(|s| i64::from(s.comment));
    let parsed_for_symbols = if entry.parsed_for_symbols { 1i64 } else { 0i64 };

    tx.execute(
        "
        INSERT INTO files (
            rel_path, mtime_secs, mtime_nanos, size, language,
            line_total, line_blank, line_comment, parsed_for_symbols, symbols_blob
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
        ON CONFLICT(rel_path) DO UPDATE SET
            mtime_secs = excluded.mtime_secs,
            mtime_nanos = excluded.mtime_nanos,
            size = excluded.size,
            language = excluded.language,
            line_total = excluded.line_total,
            line_blank = excluded.line_blank,
            line_comment = excluded.line_comment,
            parsed_for_symbols = excluded.parsed_for_symbols,
            symbols_blob = excluded.symbols_blob
        ",
        params![
            rel_path,
            entry.mtime_secs,
            mtime_nanos,
            size,
            entry.language.as_str(),
            line_total,
            line_blank,
            line_comment,
            parsed_for_symbols,
            symbols_blob,
        ],
    )?;
    Ok(())
}

fn prune_entries(tx: &Transaction<'_>, seen: &HashSet<String>) -> rusqlite::Result<()> {
    let paths = {
        let mut stmt = tx.prepare("SELECT rel_path FROM files")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.filter_map(Result::ok).collect::<Vec<_>>()
    };

    let mut delete = tx.prepare("DELETE FROM files WHERE rel_path = ?1")?;
    for path in paths {
        if !seen.contains(&path) {
            delete.execute(params![path])?;
        }
    }
    Ok(())
}

fn entry_matches(entry: &FileEntry, metadata: &Metadata, language: Language) -> bool {
    let Some((secs, nanos)) = mtime_parts(metadata) else {
        return false;
    };
    entry.mtime_secs == secs
        && entry.mtime_nanos == nanos
        && entry.size == metadata.len()
        && entry.language == language
}

fn language_from_str(value: &str) -> Option<Language> {
    match value {
        "rust" => Some(Language::Rust),
        "typescript" => Some(Language::TypeScript),
        "tsx" => Some(Language::Tsx),
        "python" => Some(Language::Python),
        "kotlin" => Some(Language::Kotlin),
        "prisma" => Some(Language::Prisma),
        "json" => Some(Language::Json),
        "yaml" => Some(Language::Yaml),
        "toml" => Some(Language::Toml),
        "markdown" => Some(Language::Markdown),
        "sql" => Some(Language::Sql),
        "html" => Some(Language::Html),
        "css" => Some(Language::Css),
        "shell" => Some(Language::Shell),
        "dockerfile" => Some(Language::Dockerfile),
        "plaintext" => Some(Language::Plaintext),
        _ => None,
    }
}

fn mtime_parts(metadata: &Metadata) -> Option<(i64, u32)> {
    let mtime = metadata.modified().ok()?;
    let duration = mtime.duration_since(UNIX_EPOCH).ok()?;
    Some((duration.as_secs() as i64, duration.subsec_nanos()))
}

fn repo_hash(repo_root: &str) -> String {
    let h = siphash13(repo_root.as_bytes(), 0, 0);
    format!("{h:016x}")
}

// Stable SipHash-1-3 with caller-chosen 128-bit key. We hand-roll it to avoid
// pulling in a crate just for cache-directory naming. Cryptographic strength
// isn't required ~ collisions just mean two repos share a cache directory,
// which the in-file repo_root field then catches and treats as empty.
fn siphash13(data: &[u8], k0: u64, k1: u64) -> u64 {
    let mut v0 = k0 ^ 0x736f_6d65_7073_6575u64;
    let mut v1 = k1 ^ 0x646f_7261_6e64_6f6du64;
    let mut v2 = k0 ^ 0x6c79_6765_6e65_7261u64;
    let mut v3 = k1 ^ 0x7465_6462_7974_6573u64;

    let block_count = data.len() / 8;
    for i in 0..block_count {
        let m = u64::from_le_bytes(data[i * 8..i * 8 + 8].try_into().unwrap());
        v3 ^= m;
        sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
        v0 ^= m;
    }

    let mut last = (data.len() as u64 & 0xff) << 56;
    let tail = &data[block_count * 8..];
    for (i, byte) in tail.iter().enumerate() {
        last |= (*byte as u64) << (i * 8);
    }
    v3 ^= last;
    sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
    v0 ^= last;

    v2 ^= 0xff;
    sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
    sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
    sip_round(&mut v0, &mut v1, &mut v2, &mut v3);

    v0 ^ v1 ^ v2 ^ v3
}

#[inline(always)]
fn sip_round(v0: &mut u64, v1: &mut u64, v2: &mut u64, v3: &mut u64) {
    *v0 = v0.wrapping_add(*v1);
    *v1 = v1.rotate_left(13);
    *v1 ^= *v0;
    *v0 = v0.rotate_left(32);
    *v2 = v2.wrapping_add(*v3);
    *v3 = v3.rotate_left(16);
    *v3 ^= *v2;
    *v0 = v0.wrapping_add(*v3);
    *v3 = v3.rotate_left(21);
    *v3 ^= *v0;
    *v2 = v2.wrapping_add(*v1);
    *v1 = v1.rotate_left(17);
    *v1 ^= *v2;
    *v2 = v2.rotate_left(32);
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::models::RangeInfo;

    struct CacheTmp {
        dir: PathBuf,
        repo_root: PathBuf,
    }

    impl CacheTmp {
        // Each test gets a unique tempdir based on pid+nanos. Tests do NOT touch
        // shared env vars (HITAGI_CACHE_DIR, HITAGI_NO_CACHE) ~ they call
        // ParseCache::open_at directly so the cache module's env handling can
        // be exercised separately without races.
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "hitagi-cachetest-{name}-{}-{unique}",
                std::process::id()
            ));
            let repo_root = dir.join("repo");
            fs::create_dir_all(&repo_root).unwrap();
            Self { dir, repo_root }
        }

        fn open(&self) -> ParseCache {
            ParseCache::open_at(self.dir.clone(), &self.repo_root)
        }

        fn cache_file(&self) -> PathBuf {
            self.dir
                .join(repo_hash(&self.repo_root.to_string_lossy()))
                .join(CACHE_FILE_NAME)
        }
    }

    impl Drop for CacheTmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn sample_symbols() -> Vec<SymbolInfo> {
        vec![SymbolInfo {
            kind: "function".to_string(),
            name: "foo".to_string(),
            qualname: "foo".to_string(),
            range: RangeInfo {
                start_byte: 0,
                end_byte: 10,
                start_line: 1,
                end_line: 2,
            },
            parent: None,
        }]
    }

    fn write_file(repo_root: &Path, rel: &str, body: &str) -> Metadata {
        let path = repo_root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, body).unwrap();
        fs::metadata(&path).unwrap()
    }

    fn update_meta(cache_file: &Path, key: &str, value: &str) {
        let conn = Connection::open(cache_file).unwrap();
        conn.execute(
            "UPDATE meta SET value = ?1 WHERE key = ?2",
            params![value, key],
        )
        .unwrap();
    }

    fn break_files_table(cache_file: &Path, replacement_schema: Option<&str>) {
        let conn = Connection::open(cache_file).unwrap();
        conn.execute_batch("DROP TABLE files").unwrap();
        if let Some(schema) = replacement_schema {
            conn.execute_batch(schema).unwrap();
        }
    }

    #[test]
    fn roundtrip_insert_save_lookup() {
        let tmp = CacheTmp::new("roundtrip");
        let metadata = write_file(&tmp.repo_root, "a.rs", "fn foo() {}");

        {
            let mut cache = tmp.open();
            cache.insert(
                "a.rs".to_string(),
                &metadata,
                Language::Rust,
                None,
                sample_symbols(),
            );
            cache.save(false);
        }

        let mut cache = tmp.open();
        let symbols = cache
            .lookup("a.rs", &metadata, Language::Rust)
            .expect("should hit after roundtrip");
        assert_eq!(symbols, sample_symbols());
    }

    #[test]
    fn lookup_misses_on_size_change() {
        let tmp = CacheTmp::new("size-change");
        let metadata = write_file(&tmp.repo_root, "a.rs", "fn foo() {}");

        let mut cache = tmp.open();
        cache.insert(
            "a.rs".to_string(),
            &metadata,
            Language::Rust,
            None,
            sample_symbols(),
        );

        let bigger = write_file(&tmp.repo_root, "a.rs", "fn foo() { 1 + 1; }");
        assert!(cache.lookup("a.rs", &bigger, Language::Rust).is_none());
    }

    #[test]
    fn lookup_misses_on_language_flip() {
        let tmp = CacheTmp::new("lang-flip");
        let metadata = write_file(&tmp.repo_root, "a.ts", "function foo() {}");

        let mut cache = tmp.open();
        cache.insert(
            "a.ts".to_string(),
            &metadata,
            Language::TypeScript,
            None,
            sample_symbols(),
        );
        // Same path, same metadata, different language => miss (covers .ts -> .tsx).
        assert!(cache.lookup("a.ts", &metadata, Language::Tsx).is_none());
    }

    #[test]
    fn version_mismatch_returns_empty() {
        let tmp = CacheTmp::new("version");
        let metadata = write_file(&tmp.repo_root, "a.rs", "fn foo() {}");

        {
            let mut cache = tmp.open();
            cache.insert(
                "a.rs".to_string(),
                &metadata,
                Language::Rust,
                None,
                sample_symbols(),
            );
            cache.save(false);
        }

        update_meta(&tmp.cache_file(), "version", "v3-9999.9.9");

        let mut cache = tmp.open();
        assert!(cache.lookup("a.rs", &metadata, Language::Rust).is_none());
    }

    #[test]
    fn repo_root_mismatch_returns_empty() {
        let tmp = CacheTmp::new("repo-root");
        let metadata = write_file(&tmp.repo_root, "a.rs", "fn foo() {}");

        {
            let mut cache = tmp.open();
            cache.insert(
                "a.rs".to_string(),
                &metadata,
                Language::Rust,
                None,
                sample_symbols(),
            );
            cache.save(false);
        }

        update_meta(&tmp.cache_file(), "repo_root", "/some/other/path");

        let mut cache = tmp.open();
        assert!(cache.lookup("a.rs", &metadata, Language::Rust).is_none());
    }

    #[test]
    fn missing_files_table_is_recreated_on_save() {
        let tmp = CacheTmp::new("missing-files-table");
        let metadata = write_file(&tmp.repo_root, "a.rs", "fn foo() {}");

        {
            let mut cache = tmp.open();
            cache.insert(
                "a.rs".to_string(),
                &metadata,
                Language::Rust,
                None,
                sample_symbols(),
            );
            cache.save(false);
        }

        break_files_table(&tmp.cache_file(), None);

        let mut cache = tmp.open();
        assert!(cache.lookup("a.rs", &metadata, Language::Rust).is_none());
        cache.insert(
            "a.rs".to_string(),
            &metadata,
            Language::Rust,
            None,
            sample_symbols(),
        );
        cache.save(false);

        let mut cache = tmp.open();
        assert_eq!(
            cache.lookup("a.rs", &metadata, Language::Rust),
            Some(sample_symbols())
        );
    }

    #[test]
    fn incompatible_files_table_is_recreated_on_save() {
        let tmp = CacheTmp::new("bad-files-table");
        let metadata = write_file(&tmp.repo_root, "a.rs", "fn foo() {}");

        {
            let mut cache = tmp.open();
            cache.insert(
                "a.rs".to_string(),
                &metadata,
                Language::Rust,
                None,
                sample_symbols(),
            );
            cache.save(false);
        }

        break_files_table(
            &tmp.cache_file(),
            Some("CREATE TABLE files (rel_path TEXT PRIMARY KEY)"),
        );

        let mut cache = tmp.open();
        assert!(cache.lookup("a.rs", &metadata, Language::Rust).is_none());
        cache.insert(
            "a.rs".to_string(),
            &metadata,
            Language::Rust,
            None,
            sample_symbols(),
        );
        cache.save(false);

        let mut cache = tmp.open();
        assert_eq!(
            cache.lookup("a.rs", &metadata, Language::Rust),
            Some(sample_symbols())
        );
    }

    #[test]
    fn save_with_prune_drops_unseen_entries() {
        let tmp = CacheTmp::new("prune");
        let m_a = write_file(&tmp.repo_root, "a.rs", "fn foo() {}");
        let m_b = write_file(&tmp.repo_root, "b.rs", "fn bar() {}");

        {
            let mut cache = tmp.open();
            cache.insert(
                "a.rs".to_string(),
                &m_a,
                Language::Rust,
                None,
                sample_symbols(),
            );
            cache.insert(
                "b.rs".to_string(),
                &m_b,
                Language::Rust,
                None,
                sample_symbols(),
            );
            cache.save(false);
        }

        let mut cache = tmp.open();
        // Only touch a.rs this run.
        let _ = cache.lookup("a.rs", &m_a, Language::Rust);
        cache.save(true);

        let mut cache = tmp.open();
        assert!(cache.lookup("a.rs", &m_a, Language::Rust).is_some());
        assert!(cache.lookup("b.rs", &m_b, Language::Rust).is_none());
    }

    #[test]
    fn save_without_prune_keeps_unseen_entries() {
        let tmp = CacheTmp::new("no-prune");
        let m_a = write_file(&tmp.repo_root, "a.rs", "fn foo() {}");
        let m_b = write_file(&tmp.repo_root, "b.rs", "fn bar() {}");

        {
            let mut cache = tmp.open();
            cache.insert(
                "a.rs".to_string(),
                &m_a,
                Language::Rust,
                None,
                sample_symbols(),
            );
            cache.insert(
                "b.rs".to_string(),
                &m_b,
                Language::Rust,
                None,
                sample_symbols(),
            );
            cache.save(false);
        }

        let mut cache = tmp.open();
        let _ = cache.lookup("a.rs", &m_a, Language::Rust);
        cache.save(false);

        let mut cache = tmp.open();
        assert!(cache.lookup("b.rs", &m_b, Language::Rust).is_some());
    }

    #[test]
    fn warm_lookup_without_pending_entries_does_not_rewrite_db() {
        let tmp = CacheTmp::new("no-rewrite");
        let metadata = write_file(&tmp.repo_root, "a.rs", "fn foo() {}");

        {
            let mut cache = tmp.open();
            cache.insert(
                "a.rs".to_string(),
                &metadata,
                Language::Rust,
                None,
                sample_symbols(),
            );
            cache.save(false);
        }

        let before = fs::metadata(tmp.cache_file()).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        let mut cache = tmp.open();
        assert!(cache.lookup("a.rs", &metadata, Language::Rust).is_some());
        cache.save(false);
        let after = fs::metadata(tmp.cache_file()).unwrap().modified().unwrap();
        assert_eq!(before, after, "warm hit + save(false) must not rewrite DB");
    }

    #[test]
    fn disabled_cache_skips_load_and_save() {
        let tmp = CacheTmp::new("disabled");
        let metadata = write_file(&tmp.repo_root, "a.rs", "fn foo() {}");

        let mut cache = ParseCache::disabled();
        cache.insert(
            "a.rs".to_string(),
            &metadata,
            Language::Rust,
            None,
            sample_symbols(),
        );
        assert!(cache.lookup("a.rs", &metadata, Language::Rust).is_none());
        cache.save(false);

        assert!(
            !tmp.cache_file().exists(),
            "disabled cache must not write {:?}",
            tmp.cache_file()
        );
    }

    fn ls(total: u32, blank: u32, comment: u32) -> LineStats {
        LineStats { total, blank, comment }
    }

    #[test]
    fn lookup_line_stats_returns_stored_value() {
        let tmp = CacheTmp::new("line-count");
        let metadata = write_file(&tmp.repo_root, "a.rs", "fn foo() {}\nfn bar() {}\n");

        {
            let mut cache = tmp.open();
            cache.insert(
                "a.rs".to_string(),
                &metadata,
                Language::Rust,
                Some(ls(2, 0, 0)),
                sample_symbols(),
            );
            cache.save(false);
        }

        let mut cache = tmp.open();
        assert_eq!(
            cache.lookup_line_stats("a.rs", &metadata, Language::Rust),
            Some(ls(2, 0, 0))
        );
    }

    #[test]
    fn lookup_line_stats_misses_on_size_change() {
        let tmp = CacheTmp::new("line-count-invalidate");
        let metadata = write_file(&tmp.repo_root, "a.rs", "fn foo() {}\n");

        let mut cache = tmp.open();
        cache.insert(
            "a.rs".to_string(),
            &metadata,
            Language::Rust,
            Some(ls(1, 0, 0)),
            sample_symbols(),
        );

        let bigger = write_file(&tmp.repo_root, "a.rs", "fn foo() {}\nfn bar() {}\n");
        assert_eq!(
            cache.lookup_line_stats("a.rs", &bigger, Language::Rust),
            None
        );
    }

    #[test]
    fn langs_only_stamp_does_not_satisfy_lookup() {
        // When langs writes a line-stats stamp for a parseable file, a
        // later outline/find/symbol call must MISS the symbol lookup so it
        // re-parses and writes a full entry. lookup_line_stats still hits.
        let tmp = CacheTmp::new("langs-stamp");
        let metadata = write_file(&tmp.repo_root, "a.rs", "fn foo() {}\n");

        let mut cache = tmp.open();
        cache.insert_lang_only("a.rs".to_string(), &metadata, Language::Rust, ls(1, 0, 0));

        assert_eq!(
            cache.lookup_line_stats("a.rs", &metadata, Language::Rust),
            Some(ls(1, 0, 0)),
            "lang-only stamp serves line counts"
        );
        assert!(
            cache.lookup("a.rs", &metadata, Language::Rust).is_none(),
            "lang-only stamp must NOT satisfy symbol lookup"
        );

        // After a real parse populates the entry, both lookups should hit.
        cache.insert(
            "a.rs".to_string(),
            &metadata,
            Language::Rust,
            Some(ls(1, 0, 0)),
            sample_symbols(),
        );
        assert_eq!(
            cache.lookup_line_stats("a.rs", &metadata, Language::Rust),
            Some(ls(1, 0, 0))
        );
        assert_eq!(
            cache.lookup("a.rs", &metadata, Language::Rust),
            Some(sample_symbols())
        );
    }

    #[test]
    fn lookup_line_stats_returns_none_when_field_unset() {
        // Forward-compat: an entry without line_stats (Option::None) should
        // miss the lookup so the caller falls back to reading the file.
        let tmp = CacheTmp::new("line-count-missing");
        let metadata = write_file(&tmp.repo_root, "a.rs", "fn foo() {}\n");

        let mut cache = tmp.open();
        cache.insert(
            "a.rs".to_string(),
            &metadata,
            Language::Rust,
            None,
            sample_symbols(),
        );

        assert_eq!(
            cache.lookup_line_stats("a.rs", &metadata, Language::Rust),
            None
        );
    }

    #[test]
    fn siphash_is_stable() {
        // Regression: changing the hash function silently invalidates every user's cache dir.
        let h1 = siphash13(b"/home/tara/Lab/urgf.online/hitagi", 0, 0);
        let h2 = siphash13(b"/home/tara/Lab/urgf.online/hitagi", 0, 0);
        assert_eq!(h1, h2);
        assert_ne!(h1, siphash13(b"/home/tara/Lab/urgf.online/cassia", 0, 0));
    }
}
