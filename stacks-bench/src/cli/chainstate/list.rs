use anyhow::Result;

use crate::cli::common::{Align, CliContext, Table};

#[derive(clap::Args, Debug)]
pub struct ListArgs {
    /// Maximum number of chainstates to display.
    #[arg(long, default_value_t = 50)]
    pub limit: usize,

    /// Output as JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

impl ListArgs {
    pub async fn exec(&self, ctx: &CliContext) -> Result<()> {
        let app_db = ctx.app_db();
        let mut chainstates = app_db.list_chainstates().await?;

        chainstates.truncate(self.limit);

        if chainstates.is_empty() {
            cliclack::log::info("No chainstates found.")?;
            return Ok(());
        }

        // Resolve additional info for each chainstate
        struct Row {
            id: i32,
            network: String,
            chain_id: i64,
            tip_height: i64,
            tip_hash: String,
            epochs_hash: String,
            run_count: i64,
        }

        let mut rows = Vec::with_capacity(chainstates.len());
        for cs in &chainstates {
            let network = app_db.get_network_name(cs.network_id).await?;
            let run_count = app_db.count_benchmark_runs_for_chainstate(cs.id).await?;
            rows.push(Row {
                id: cs.id,
                network,
                chain_id: cs.chain_id,
                tip_height: cs.tip_height,
                tip_hash: hex::encode(&cs.tip_index_hash),
                epochs_hash: hex::encode(&cs.epochs_hash),
                run_count,
            });
        }

        // --- JSON output ---
        if self.json {
            #[derive(serde::Serialize)]
            struct ChainstateJson {
                id: i32,
                network: String,
                chain_id: i64,
                tip_height: i64,
                tip_hash: String,
                epochs_hash: String,
                runs: i64,
            }

            let items: Vec<ChainstateJson> = rows
                .iter()
                .map(|r| ChainstateJson {
                    id: r.id,
                    network: r.network.clone(),
                    chain_id: r.chain_id,
                    tip_height: r.tip_height,
                    tip_hash: r.tip_hash.clone(),
                    epochs_hash: r.epochs_hash.clone(),
                    runs: r.run_count,
                })
                .collect();

            println!("{}", serde_json::to_string_pretty(&items)?);
            return Ok(());
        }

        // --- Table output ---
        let mut table = Table::new()
            .col("ID", Align::Right)
            .col("Network", Align::Left)
            .col("Chain ID", Align::Right)
            .col("Tip Height", Align::Right)
            .col_with("Tip Hash", Align::Left, 16, Some(16))
            .col("Runs", Align::Right);

        for r in &rows {
            let short_hash = r.tip_hash[..r.tip_hash.len().min(16)].to_string();
            table.row(vec![
                r.id.to_string(),
                r.network.clone(),
                r.chain_id.to_string(),
                r.tip_height.to_string(),
                short_hash,
                r.run_count.to_string(),
            ]);
        }

        table.print_with_footer("chainstate", self.limit)?;
        Ok(())
    }
}
