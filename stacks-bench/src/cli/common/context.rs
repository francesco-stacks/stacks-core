use stacks_bench::db::app::AppDb;
use stacks_bench::paths::AppDataDir;

pub struct CliContext {
    /// The path to the application database (SQLite). If not specified, the database
    /// will be created in the same directory as the `stacks-bench` binary.
    app_data_dir: AppDataDir,
    /// The application database.
    app_db: AppDb,
}

pub const SUCCESS_ICON: &str = "✔";
#[allow(unused)]
pub const FAILURE_ICON: &str = "✘";

macro_rules! fmt_success {
    ($($arg:tt)*) => {{
        format!(
            "{} {}",
            ::console::style($crate::cli::common::SUCCESS_ICON).green(),
            format_args!($($arg)*)
        )
    }};
}

#[allow(unused)]
macro_rules! fmt_failure {
    ($($arg:tt)*) => {{
        format!(
            "{} {}",
            ::console::style($crate::cli::common::FAILURE_ICON).red(),
            format_args!($($arg)*)
        )
    }};
}

impl CliContext {
    pub fn new(app_data_dir: AppDataDir, app_db: AppDb) -> Self {
        Self {
            app_data_dir,
            app_db,
        }
    }

    pub fn app_data_dir(&self) -> &AppDataDir {
        &self.app_data_dir
    }

    pub fn app_db(&self) -> AppDb {
        self.app_db.clone()
    }
}
