//! Bootstrap `HF_HOME` so hf-hub downloads land under mimi's own cache
//! root instead of `~/.cache/huggingface`. Keeps everything we manage under
//! one dir; users can `mimi cache clear --all` and actually wipe the
//! model files too.

use std::path::PathBuf;
use std::sync::Once;

use crate::cache::cache_root;

static HF_HOME_INIT: Once = Once::new();

/// Set `HF_HOME=$XDG_CACHE_HOME/mimi/models/` if the user hasn't already
/// set it. Called from any command that may load an embedding model.
///
/// Safety: env mutation is racy with concurrent reads from other threads.
/// We gate behind `Once` so the variable is set exactly once per process,
/// before any `hf-hub` API is constructed.
pub fn ensure_hf_home() {
    HF_HOME_INIT.call_once(|| {
        if std::env::var_os("HF_HOME").is_some() {
            return;
        }
        let Some(root) = mimi_models_dir() else {
            return;
        };
        // SAFETY: `Once` guarantees this is the only writer in this process,
        // and we run before any hf-hub API construction.
        unsafe {
            std::env::set_var("HF_HOME", root);
        }
    });
}

pub fn mimi_models_dir() -> Option<PathBuf> {
    cache_root().map(|root| root.join("models"))
}
