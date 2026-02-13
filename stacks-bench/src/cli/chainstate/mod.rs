pub mod index;
pub mod list;
pub mod remove;

use anyhow::Result;
use clap::Subcommand;
use index::IndexArgs;
use list::ListArgs;
use remove::RemoveArgs;

use crate::cli::common::CliContext;

#[derive(Subcommand, Debug)]
pub enum ChainstateCommands {
    /// Index a range of blocks from the node database
    Index(IndexArgs),
    /// List indexed chainstates
    #[command(alias = "ls")]
    List(ListArgs),
    /// Delete one or more chainstates and all associated data
    #[command(alias = "rm")]
    Remove(RemoveArgs),
}

#[derive(clap::Args, Debug)]
pub struct ChainstateArgs {
    #[command(subcommand)]
    command: ChainstateCommands,
}

impl ChainstateArgs {
    pub async fn exec(&self, ctx: &CliContext) -> Result<()> {
        match &self.command {
            ChainstateCommands::Index(args) => args.exec(ctx).await,
            ChainstateCommands::List(args) => args.exec(ctx).await,
            ChainstateCommands::Remove(args) => args.exec(ctx).await,
        }
    }
}
