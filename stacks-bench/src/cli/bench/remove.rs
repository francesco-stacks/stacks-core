use anyhow::Result;

use crate::cli::common::CliContext;

#[derive(clap::Args, Debug)]
pub struct RemoveArgs {
    #[arg(long, alias = "id")]
    pub run_id: u32,
}

impl RemoveArgs {
    pub async fn exec(&self, ctx: &CliContext) -> Result<()> {
        let _app_db = ctx.app_db();

        todo!()
    }
}
