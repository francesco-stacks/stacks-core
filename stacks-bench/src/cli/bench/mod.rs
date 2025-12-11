use crate::cli::common::CliContext;

pub mod remove;
pub mod run;

#[derive(clap::Subcommand, Debug)]
pub enum BenchCommand {
    Run(run::RunArgs),
    Remove(remove::RemoveArgs),
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
            BenchCommand::Remove(args) => args.exec(ctx).await,
        }
    }
}
