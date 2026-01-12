use std::path::PathBuf;

use anyhow::Result;
use bench::BenchArgs;
use chainstate::ChainstateArgs;
use clap::{Parser, Subcommand};
use console::style;
use metabase::MetabaseArgs;
use stacks_bench::db::app::AppDb;
use stacks_bench::paths::AppDataDir;

use crate::cli::common::CliContext;

#[macro_use]
pub mod common;
pub mod bench;
pub mod chainstate;
pub mod metabase;

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Run the benchmark
    Bench(BenchArgs),
    /// Manage chainstate data
    Chainstate(ChainstateArgs),
    /// Launch a pre-configured Metabase instance to analyze results
    Metabase(MetabaseArgs),
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
        cliclack::intro(style(" stacks-bench ").on_cyan().black())?;

        // Use AppDataPath to resolve locations
        let app_data = AppDataDir::resolve_from_opt(self.app_data_dir.as_ref())?;

        let app_db_path = app_data.app_db_path();
        let app_db = AppDb::open(&app_db_path).await?;

        let ctx = CliContext::new(app_data, app_db);

        let result = match &self.command {
            Commands::Bench(args) => args.exec(&ctx).await,
            Commands::Chainstate(args) => args.exec(&ctx).await,
            Commands::Metabase(args) => args.exec(&ctx).await,
        };

        match result {
            Ok(_) => {
                cliclack::outro("Finished")?;
                Ok(())
            }
            Err(e) => {
                cliclack::outro_cancel(format!("Command failed: {e:?}"))?;
                Err(e)
            }
        }
    }
}
