use anyhow::{Context, Result};
use console::style;

use super::run::RunArgs;
use crate::cli::common::{CliContext, fmt_run_label, fmt_run_name_suffix};

#[derive(clap::Args, Debug)]
pub struct RerunArgs {
    /// The ID of the benchmark run to re-run. If omitted, an interactive
    /// selector is shown with all available runs.
    #[arg(long, alias = "id")]
    pub run_id: Option<u32>,
}

impl RerunArgs {
    pub async fn exec(&self, ctx: &CliContext) -> Result<()> {
        let app_db = ctx.app_db();

        // Resolve the run ID
        let run_id: i32 = if let Some(id) = self.run_id {
            id as i32
        } else {
            // Interactive mode: list runs and let the user pick one
            let runs = app_db.list_benchmark_runs().await?;
            if runs.is_empty() {
                cliclack::log::info("No benchmark runs found.")?;
                return Ok(());
            }

            let mut select = cliclack::select(format!(
                "Select a benchmark run to re-run ({} available)",
                runs.len()
            ));
            for run in &runs {
                select = select.item(run.id, format!("Run {}", run.id), fmt_run_label(run));
            }
            select.filter_mode().interact()?
        };

        // Look up the run
        let run = app_db
            .get_benchmark_run(run_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Benchmark run {} not found", run_id))?;

        cliclack::log::step(format!(
            "Re-running benchmark run {}{} started at {}",
            style(run.id).bold(),
            fmt_run_name_suffix(&run),
            run.start_time.format("%Y-%m-%d %H:%M:%S"),
        ))?;

        // Deserialize the original RunArgs from the stored JSON
        let run_args: RunArgs = serde_json::from_str(&run.args_json).with_context(|| {
            format!(
                "Failed to deserialize args for run {} — stored JSON: {}",
                run.id, &run.args_json
            )
        })?;

        // Dispatch to the standard run command
        run_args.exec(ctx).await
    }
}
