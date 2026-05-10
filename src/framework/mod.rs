//! Framework-aware queries.
//!
//! Each framework lives in its own submodule; the public surface is a small set
//! of `<framework>_<action>` functions that the CLI dispatches to. Adding a new
//! framework means a sibling file here plus another match arm in the CLI.

mod next;

pub use next::{next_info, next_list_layouts, next_list_routes, next_list_server_actions};
