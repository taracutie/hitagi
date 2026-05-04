use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ffi::OsString,
    fs::Metadata,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use serde::{Deserialize, Serialize};

use crate::{lang::Language, models::SymbolInfo};

// Bumping the crate version invalidates all caches ~ cheapest proxy for
// "visitor logic in parser.rs may have changed shape". Schema bumps just
// change the v<N> prefix. Mismatch on either falls back to an empty cache.
//
// v2 added `line_count` to FileEntry and started inserting lang-only entries
// (symbols=empty) for non-parseable files so `langs` can serve from cache.
const CACHE_VERSION_KEY: &str = concat!("v2-", env!("CARGO_PKG_VERSION"));
const CACHE_FILE_NAME: &str = "index.v2.bin";

#[derive(Serialize, Deserialize)]
struct CacheFile {
    version: String,
    repo_root: String,
    entries: HashMap<String, FileEntry>,
}

#[derive(Serialize, Deserialize, Clone)]
struct FileEntry {
    mtime_secs: i64,
    mtime_nanos: u32,
    size: u64,
    language: Language,
    /// Newline count of the file's content as last seen. Capped at u32::MAX
    /// (no real source file approaches that). Both parsed entries and
    /// lang-only stamps populate this so `langs` warm runs are O(stat).
    #[serde(default)]
    line_count: Option<u32>,
    /// True when `symbols` came from `parse_source`. False when this entry
    /// was stamped by `langs` (line_count only); the symbols vec is empty
    /// in that case and the next outline/find/symbol call must re-parse.
    /// Defaulting to false on legacy entries is safe: they get re-parsed
    /// once and upgraded.
    #[serde(default)]
    parsed_for_symbols: bool,
    symbols: Vec<SymbolInfo>,
}

pub struct ParseCache {
    cache_dir: PathBuf,
    repo_root: String,
    entries: HashMap<String, FileEntry>,
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
        let entries = load_entries(&cache_dir, &repo_root).unwrap_or_default();
        Self {
            cache_dir,
            repo_root,
            entries,
            seen: HashSet::new(),
            enabled: true,
        }
    }

    fn empty(cache_dir: PathBuf, repo_root: String, enabled: bool) -> Self {
        Self {
            cache_dir,
            repo_root,
            entries: HashMap::new(),
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
        Some(entry.symbols.clone())
    }

    /// Returns the cached line count when (mtime, size, language) match.
    /// Used by `langs` to skip re-reading every file's bytes on warm runs.
    /// Lang-only entries (symbols.is_empty()) populate this for non-parseable
    /// languages too.
    pub fn lookup_line_count(
        &mut self,
        rel_path: &str,
        metadata: &Metadata,
        language: Language,
    ) -> Option<u32> {
        let entry = self.lookup_entry(rel_path, metadata, language)?;
        entry.line_count
    }

    fn lookup_entry(
        &mut self,
        rel_path: &str,
        metadata: &Metadata,
        language: Language,
    ) -> Option<&FileEntry> {
        if !self.enabled {
            return None;
        }
        self.seen.insert(rel_path.to_string());

        let entry = self.entries.get(rel_path)?;
        let (secs, nanos) = mtime_parts(metadata)?;
        if entry.mtime_secs == secs
            && entry.mtime_nanos == nanos
            && entry.size == metadata.len()
            && entry.language == language
        {
            Some(entry)
        } else {
            None
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
        line_count: Option<u32>,
        symbols: Vec<SymbolInfo>,
    ) {
        self.insert_entry(rel_path, metadata, language, line_count, symbols, true);
    }

    /// Insert a langs-only stamp: line_count without parsed symbols. The
    /// next outline/find/symbol call for this path treats it as a cache
    /// miss and re-parses, replacing this entry with a full one.
    pub fn insert_lang_only(
        &mut self,
        rel_path: String,
        metadata: &Metadata,
        language: Language,
        line_count: u32,
    ) {
        self.insert_entry(
            rel_path,
            metadata,
            language,
            Some(line_count),
            Vec::new(),
            false,
        );
    }

    fn insert_entry(
        &mut self,
        rel_path: String,
        metadata: &Metadata,
        language: Language,
        line_count: Option<u32>,
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
        self.entries.insert(
            rel_path,
            FileEntry {
                mtime_secs: secs,
                mtime_nanos: nanos,
                size: metadata.len(),
                language,
                line_count,
                parsed_for_symbols,
                symbols,
            },
        );
    }

    /// Persist to disk. When `prune_unseen` is true, drop entries not visited
    /// during this run ~ only safe when the walk covered the whole repo.
    /// Failures are silent: a stale or missing cache must never break a command.
    pub fn save(mut self, prune_unseen: bool) {
        if !self.enabled {
            return;
        }
        if prune_unseen {
            self.entries.retain(|key, _| self.seen.contains(key));
        }
        let _ = write_entries(&self.cache_dir, &self.repo_root, &self.entries);
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

        let Ok(bytes) = std::fs::read(&file) else {
            return inspection;
        };
        let Ok(decoded) = bincode::deserialize::<CacheFile>(&bytes) else {
            return inspection;
        };

        inspection.version_match = decoded.version == CACHE_VERSION_KEY;
        inspection.repo_root_match = decoded.repo_root == repo_root_str;
        inspection.stored_version = Some(decoded.version);
        inspection.stored_repo_root = Some(decoded.repo_root);
        inspection.entry_count = decoded.entries.len();
        for entry in decoded.entries.values() {
            *inspection
                .languages
                .entry(entry.language.as_str().to_string())
                .or_insert(0) += 1;
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

fn load_entries(cache_dir: &Path, expected_repo_root: &str) -> Option<HashMap<String, FileEntry>> {
    let bytes = std::fs::read(cache_dir.join(CACHE_FILE_NAME)).ok()?;
    let file: CacheFile = bincode::deserialize(&bytes).ok()?;
    if file.version != CACHE_VERSION_KEY || file.repo_root != expected_repo_root {
        return None;
    }
    Some(file.entries)
}

fn write_entries(
    cache_dir: &Path,
    repo_root: &str,
    entries: &HashMap<String, FileEntry>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(cache_dir)?;
    let target = cache_dir.join(CACHE_FILE_NAME);
    let tmp = cache_dir.join(format!("{CACHE_FILE_NAME}.tmp.{}", std::process::id()));

    let file = CacheFile {
        version: CACHE_VERSION_KEY.to_string(),
        repo_root: repo_root.to_string(),
        entries: entries.clone(),
    };
    let bytes = bincode::serialize(&file)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, &bytes)?;
    if let Err(error) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
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
        time::{SystemTime, UNIX_EPOCH},
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

        let cache_file = tmp.cache_file();
        let bytes = fs::read(&cache_file).unwrap();
        let mut decoded: CacheFile = bincode::deserialize(&bytes).unwrap();
        decoded.version = "v2-9999.9.9".to_string();
        let rewritten = bincode::serialize(&decoded).unwrap();
        fs::write(&cache_file, &rewritten).unwrap();

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

        let cache_file = tmp.cache_file();
        let bytes = fs::read(&cache_file).unwrap();
        let mut decoded: CacheFile = bincode::deserialize(&bytes).unwrap();
        decoded.repo_root = "/some/other/path".to_string();
        let rewritten = bincode::serialize(&decoded).unwrap();
        fs::write(&cache_file, &rewritten).unwrap();

        let mut cache = tmp.open();
        assert!(cache.lookup("a.rs", &metadata, Language::Rust).is_none());
    }

    #[test]
    fn save_with_prune_drops_unseen_entries() {
        let tmp = CacheTmp::new("prune");
        let m_a = write_file(&tmp.repo_root, "a.rs", "fn foo() {}");
        let m_b = write_file(&tmp.repo_root, "b.rs", "fn bar() {}");

        {
            let mut cache = tmp.open();
            cache.insert("a.rs".to_string(), &m_a, Language::Rust, None, sample_symbols());
            cache.insert("b.rs".to_string(), &m_b, Language::Rust, None, sample_symbols());
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
            cache.insert("a.rs".to_string(), &m_a, Language::Rust, None, sample_symbols());
            cache.insert("b.rs".to_string(), &m_b, Language::Rust, None, sample_symbols());
            cache.save(false);
        }

        let mut cache = tmp.open();
        let _ = cache.lookup("a.rs", &m_a, Language::Rust);
        cache.save(false);

        let mut cache = tmp.open();
        assert!(cache.lookup("b.rs", &m_b, Language::Rust).is_some());
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

    #[test]
    fn lookup_line_count_returns_stored_value() {
        let tmp = CacheTmp::new("line-count");
        let metadata = write_file(&tmp.repo_root, "a.rs", "fn foo() {}\nfn bar() {}\n");

        {
            let mut cache = tmp.open();
            cache.insert(
                "a.rs".to_string(),
                &metadata,
                Language::Rust,
                Some(2),
                sample_symbols(),
            );
            cache.save(false);
        }

        let mut cache = tmp.open();
        assert_eq!(
            cache.lookup_line_count("a.rs", &metadata, Language::Rust),
            Some(2)
        );
    }

    #[test]
    fn lookup_line_count_misses_on_size_change() {
        let tmp = CacheTmp::new("line-count-invalidate");
        let metadata = write_file(&tmp.repo_root, "a.rs", "fn foo() {}\n");

        let mut cache = tmp.open();
        cache.insert(
            "a.rs".to_string(),
            &metadata,
            Language::Rust,
            Some(1),
            sample_symbols(),
        );

        let bigger = write_file(&tmp.repo_root, "a.rs", "fn foo() {}\nfn bar() {}\n");
        assert_eq!(
            cache.lookup_line_count("a.rs", &bigger, Language::Rust),
            None
        );
    }

    #[test]
    fn langs_only_stamp_does_not_satisfy_lookup() {
        // When langs writes a line-count stamp for a parseable file, a
        // later outline/find/symbol call must MISS the symbol lookup so it
        // re-parses and writes a full entry. lookup_line_count still hits.
        let tmp = CacheTmp::new("langs-stamp");
        let metadata = write_file(&tmp.repo_root, "a.rs", "fn foo() {}\n");

        let mut cache = tmp.open();
        cache.insert_lang_only("a.rs".to_string(), &metadata, Language::Rust, 1);

        assert_eq!(
            cache.lookup_line_count("a.rs", &metadata, Language::Rust),
            Some(1),
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
            Some(1),
            sample_symbols(),
        );
        assert_eq!(
            cache.lookup_line_count("a.rs", &metadata, Language::Rust),
            Some(1)
        );
        assert_eq!(
            cache.lookup("a.rs", &metadata, Language::Rust),
            Some(sample_symbols())
        );
    }

    #[test]
    fn lookup_line_count_returns_none_when_field_unset() {
        // Forward-compat: an entry without line_count (Option::None) should
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
            cache.lookup_line_count("a.rs", &metadata, Language::Rust),
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
