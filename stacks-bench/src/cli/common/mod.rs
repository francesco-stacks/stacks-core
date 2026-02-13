// Macros must be defined before submodules that use them, so `context`
// (which contains the macro definitions) comes first.
#[macro_use]
mod context;
mod args;
mod cleanup;
mod format;
mod progress;
mod setup;
mod table;

pub use args::{IndexerArgs, TxIdArg};
pub use cleanup::run_db_cleanup;
#[allow(unused_imports)] // FAILURE_ICON referenced by fmt_failure! macro
pub use context::{CliContext, FAILURE_ICON, SUCCESS_ICON};
pub use format::{
    fmt_duration, fmt_relative_time, fmt_run_label, fmt_run_name_suffix, fmt_u64_thousands,
    parse_since,
};
pub use progress::run_indexer_progress_ui;
pub use setup::{create_shadow_dir, get_git_hash, setup_bench_env, setup_bench_env_and_plan};
pub use table::{Align, Table};
