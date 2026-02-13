use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use bench::BenchArgs;
use chainstate::ChainstateArgs;
use clap::{Parser, Subcommand};
use console::style;
use explorer::ExplorerArgs;
use metabase::MetabaseArgs;
use stacks_bench::db::app::AppDb;
use stacks_bench::paths::AppDataDir;

use crate::cli::common::CliContext;

#[macro_use]
pub mod common;
pub mod bench;
pub mod chainstate;
pub mod explorer;
pub mod metabase;
mod theme;

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Run the benchmark
    Bench(BenchArgs),
    /// Manage chainstate data
    Chainstate(ChainstateArgs),
    /// Launch a pre-configured Metabase instance to analyze results
    Metabase(MetabaseArgs),
    /// Launch the profiler explorer web UI
    Explorer(ExplorerArgs),
}

#[derive(Parser, Debug)]
#[command(name = "stacks-bench", about)]
pub struct Cli {
    /// The path to the application database (SQLite). If not specified, the database
    /// will be created in the same directory as the `stacks-bench` binary.
    #[arg(long = "db", value_name = "APP_DATA_DIR")]
    pub app_data_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Commands,
}

impl Cli {
    pub async fn exec(&self) -> Result<()> {
        let started_at = Instant::now();
        cliclack::set_theme(theme::CliTheme);
        cliclack::intro(style(" stacks-bench ").on_cyan().black())?;

        // Use AppDataPath to resolve locations
        let app_data = AppDataDir::resolve_from_opt(self.app_data_dir.as_ref())?;

        let app_db_path = app_data.app_db_path();
        let app_db = AppDb::open(&app_db_path).await.inspect_err(|e| {
            let msg = format!(
                "Failed to open app database at {}: {e}",
                app_db_path.display()
            );
            cliclack::log::error(msg).ok();
        })?;

        let ctx = CliContext::new(app_data, app_db);

        let result = match &self.command {
            Commands::Bench(args) => args.exec(&ctx).await,
            Commands::Chainstate(args) => args.exec(&ctx).await,
            Commands::Metabase(args) => args.exec(&ctx).await,
            Commands::Explorer(args) => args.exec(&ctx).await,
        };

        let exec_duration = started_at.elapsed();
        let secs = exec_duration.as_secs();
        let hh = secs / 3600;
        let mm = (secs % 3600) / 60;
        let ss = secs % 60;
        let exec_duration_str = format!("{:02}:{:02}:{:02}", hh, mm, ss);

        match result {
            Ok(_) => {
                let finished = style("Finished").green().bold();
                let timing = style(format!("in {exec_duration_str} ({secs}s)"))
                    .dim()
                    .italic();
                cliclack::outro(format!("{finished} {timing}"))?;
                Ok(())
            }
            Err(e) => {
                let failed = style("Failed").red().bold();
                let timing = style(format!("after {exec_duration_str} ({secs}s)"))
                    .dim()
                    .italic();
                cliclack::outro_cancel(format!("{failed} {timing}\n  {e:?}"))?;
                Err(e)
            }
        }
    }
}
