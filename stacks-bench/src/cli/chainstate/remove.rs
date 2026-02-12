use anyhow::{Result, bail};
use console::style;
use stacks_bench::db::app::CheckpointMode;

use crate::cli::common::CliContext;

#[derive(clap::Args, Debug)]
pub struct RemoveArgs {
    /// The ID of the chainstate(s) to delete. If omitted, an interactive
    /// selector is shown with all available chainstates.
    #[arg(long, alias = "id")]
    pub chainstate_id: Option<Vec<u32>>,

    /// Skip the confirmation prompt.
    #[arg(long, short = 'y', default_value_t = false)]
    pub yes: bool,
}

impl RemoveArgs {
    pub async fn exec(&self, ctx: &CliContext) -> Result<()> {
        let mut app_db = ctx.app_db();

        // Resolve the set of chainstate IDs to delete
        let chainstate_ids: Vec<i32> = if let Some(ids) = &self.chainstate_id {
            let mut converted: Vec<i32> = ids
                .iter()
                .map(|&id| {
                    i32::try_from(id).map_err(|_| {
                        anyhow::anyhow!("chainstate ID {id} exceeds maximum ({})", i32::MAX)
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            converted.sort_unstable();
            converted.dedup();
            converted
        } else {
            // Interactive mode: list chainstates and let the user pick
            let chainstates = app_db.list_chainstates().await?;
            if chainstates.is_empty() {
                cliclack::log::info("No chainstates found.")?;
                return Ok(());
            }

            let mut select = cliclack::multiselect(format!(
                "Select chainstates to delete ({} available)",
                chainstates.len()
            ));
            for cs in &chainstates {
                let network_name = app_db.get_network_name(cs.network_id).await?;
                let run_count = app_db.count_benchmark_runs_for_chainstate(cs.id).await?;
                select = select.item(
                    cs.id,
                    format!("Chainstate {}", cs.id),
                    format!(
                        "{network_name} | chain_id={} | tip_height={} | {run_count} run{}",
                        cs.chain_id,
                        cs.tip_height,
                        if run_count == 1 { "" } else { "s" }
                    ),
                );
            }
            let chosen: Vec<i32> = select.filter_mode().interact()?;
            if chosen.is_empty() {
                cliclack::log::info("No chainstates selected.")?;
                return Ok(());
            }
            chosen
        };

        // Look up each chainstate to validate and display context
        let mut chainstates = Vec::with_capacity(chainstate_ids.len());
        for &id in &chainstate_ids {
            match app_db.get_chainstate(id).await? {
                Some(cs) => chainstates.push(cs),
                None => bail!("Chainstate {} not found", id),
            }
        }

        for cs in &chainstates {
            let network_name = app_db.get_network_name(cs.network_id).await?;
            let run_count = app_db.count_benchmark_runs_for_chainstate(cs.id).await?;
            cliclack::log::step(format!(
                "Chainstate {} — {} | tip_height={} | {run_count} associated run{}",
                style(cs.id).bold(),
                network_name,
                cs.tip_height,
                if run_count == 1 { "" } else { "s" },
            ))?;
        }

        if !self.yes {
            let label = if chainstates.len() == 1 {
                format!(
                    "Delete chainstate {} and all associated data (including benchmark runs)?",
                    chainstates[0].id
                )
            } else {
                format!(
                    "Delete {} chainstates and all associated data (including benchmark runs)?",
                    chainstates.len()
                )
            };
            if !cliclack::confirm(label).interact()? {
                cliclack::log::info("Aborted.")?;
                return Ok(());
            }
        }

        // Delete each chainstate with its own spinner inside a multi_progress
        let multi = cliclack::multi_progress(format!(
            "Deleting {} chainstate{}",
            chainstates.len(),
            if chainstates.len() == 1 { "" } else { "s" }
        ));

        for cs in &chainstates {
            let spinner = multi.add(cliclack::spinner());
            spinner.start(format!("Deleting chainstate {}…", cs.id));
            app_db.delete_chainstate(cs.id).await?;
            spinner.stop(format!(
                "{} Chainstate {} deleted",
                style("✔").green(),
                cs.id,
            ));
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
