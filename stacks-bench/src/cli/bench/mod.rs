use crate::cli::common::CliContext;

pub mod list;
pub mod remove;
pub mod rerun;
pub mod run;

#[derive(clap::Subcommand, Debug)]
pub enum BenchCommand {
    Run(run::RunArgs),
    /// Re-run an existing benchmark using its original parameters.
    Rerun(rerun::RerunArgs),
    #[command(alias = "rm")]
    Remove(remove::RemoveArgs),
    #[command(alias = "ls")]
    List(list::ListArgs),
}

#[derive(clap::Args, Debug)]
pub struct BenchArgs {
    #[command(subcommand)]
    pub command: BenchCommand,
}

impl BenchArgs {
    pub async fn exec(&self, ctx: &CliContext) -> anyhow::Result<()> {
        match &self.command {
            BenchCommand::Run(args) => args.exec(ctx).await,
            BenchCommand::Rerun(args) => args.exec(ctx).await,
            BenchCommand::Remove(args) => args.exec(ctx).await,
            BenchCommand::List(args) => args.exec(ctx).await,
        }
    }
}
