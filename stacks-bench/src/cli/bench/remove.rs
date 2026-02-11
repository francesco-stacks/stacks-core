use anyhow::{Result, bail};
use console::style;
use stacks_bench::db::app::CheckpointMode;

use crate::cli::common::{CliContext, format_run_label, format_run_name_suffix};

#[derive(clap::Args, Debug)]
pub struct RemoveArgs {
    /// The ID of the benchmark run(s) to delete. If omitted, an interactive
    /// selector is shown with all available runs.
    #[arg(long, alias = "id")]
    pub run_id: Option<Vec<u32>>,

    /// Skip the confirmation prompt.
    #[arg(long, short = 'y', default_value_t = false)]
    pub yes: bool,
}

impl RemoveArgs {
    pub async fn exec(&self, ctx: &CliContext) -> Result<()> {
        let mut app_db = ctx.app_db();

        // Resolve the set of run IDs to delete
        let run_ids: Vec<i32> = if let Some(ids) = &self.run_id {
            ids.iter().map(|&id| id as i32).collect()
        } else {
            // Interactive mode: list runs and let the user pick
            let runs = app_db.list_benchmark_runs().await?;
            if runs.is_empty() {
                cliclack::log::info("No benchmark runs found.")?;
                return Ok(());
            }

            let mut select = cliclack::multiselect(format!(
                "Select benchmark runs to delete ({} available)",
                runs.len()
            ));
            for run in &runs {
                select = select.item(run.id, format!("Run {}", run.id), format_run_label(run));
            }
            let chosen: Vec<i32> = select.filter_mode().interact()?;
            if chosen.is_empty() {
                cliclack::log::info("No runs selected.")?;
                return Ok(());
            }
            chosen
        };

        // Look up each run to validate and display context
        let mut runs = Vec::with_capacity(run_ids.len());
        for &id in &run_ids {
            match app_db.get_benchmark_run(id).await? {
                Some(r) => runs.push(r),
                None => bail!("Benchmark run {} not found", id),
            }
        }

        for run in &runs {
            cliclack::log::step(format!(
                "Run {}{} started at {}",
                style(run.id).bold(),
                format_run_name_suffix(run),
                run.start_time.format("%Y-%m-%d %H:%M:%S"),
            ))?;
        }

        if !self.yes {
            let label = if runs.len() == 1 {
                format!(
                    "Delete benchmark run {} and all associated data?",
                    runs[0].id
                )
            } else {
                format!(
                    "Delete {} benchmark runs and all associated data?",
                    runs.len()
                )
            };
            if !cliclack::confirm(label).interact()? {
                cliclack::log::info("Aborted.")?;
                return Ok(());
            }
        }

        // Delete each run with its own spinner inside a multi_progress
        let multi = cliclack::multi_progress(format!(
            "Deleting {} benchmark run{}",
            runs.len(),
            if runs.len() == 1 { "" } else { "s" }
        ));

        for run in &runs {
            let spinner = multi.add(cliclack::spinner());
            spinner.start(format!("Deleting run {}…", run.id));
            app_db.delete_benchmark_run(run.id).await?;
            spinner.stop(format!("{} Run {} deleted", style("✔").green(), run.id,));
        }

        multi.stop();

        // Checkpoint + vacuum to reclaim space
        let maint_spinner = cliclack::spinner();
        maint_spinner.start("Checkpointing database…");
        app_db.checkpoint(CheckpointMode::Truncate).await?;
        maint_spinner.stop(format!("{} Checkpoint complete", style("✔").green()));

        let vacuum_spinner = cliclack::spinner();
        vacuum_spinner.start("Vacuuming database…");
        app_db.vacuum().await?;
        vacuum_spinner.stop(format!("{} Vacuum complete", style("✔").green()));

        Ok(())
    }
}
