use anyhow::Result;
use chrono::Utc;
use console::style;

use crate::cli::common::{Align, CliContext, Table, fmt_duration, fmt_relative_time, parse_since};

#[derive(clap::Args, Debug)]
pub struct ListArgs {
    /// Show only runs from today (local time).
    #[arg(long, conflicts_with = "since")]
    pub today: bool,

    /// Show runs from the last N duration (e.g. `10m`, `2h`, `1d6h`).
    #[arg(long, conflicts_with = "today", value_name = "DURATION")]
    pub since: Option<String>,

    /// Show only incomplete (in-progress or failed) runs. By default these
    /// are hidden.
    #[arg(long)]
    pub incomplete: bool,

    /// Show all runs regardless of completion status (overrides the default
    /// filter that hides incomplete runs).
    #[arg(long, short = 'a', conflicts_with = "incomplete")]
    pub all: bool,

    /// Filter by run name (substring match, case-insensitive).
    #[arg(long, short = 'n', value_name = "PATTERN")]
    pub name: Option<String>,

    /// Maximum number of runs to display.
    #[arg(long, default_value_t = 50)]
    pub limit: usize,

    /// Output as JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

impl ListArgs {
    pub async fn exec(&self, ctx: &CliContext) -> Result<()> {
        let app_db = ctx.app_db();
        let mut runs = app_db.list_benchmark_runs().await?;

        // --- Completion status filter (default: completed only) ---
        if self.incomplete {
            runs.retain(|r| r.end_time.is_none());
        } else if !self.all {
            runs.retain(|r| r.end_time.is_some());
        }

        // --- Time-based filters ---
        if self.today {
            let today_start = Utc::now()
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .expect("valid midnight");
            runs.retain(|r| r.start_time >= today_start);
        } else if let Some(since_str) = &self.since {
            let duration = parse_since(since_str)?;
            let cutoff = Utc::now().naive_utc() - duration;
            runs.retain(|r| r.start_time >= cutoff);
        }

        // --- Name filter ---
        if let Some(pattern) = &self.name {
            let pat = pattern.to_lowercase();
            runs.retain(|r| {
                r.run_name
                    .as_deref()
                    .map(|n| n.to_lowercase().contains(&pat))
                    .unwrap_or(false)
            });
        }

        // --- Limit ---
        runs.truncate(self.limit);

        if runs.is_empty() {
            cliclack::log::info("No matching benchmark runs found.")?;
            return Ok(());
        }

        // --- JSON output ---
        if self.json {
            #[derive(serde::Serialize)]
            struct RunJson {
                id: i32,
                name: Option<String>,
                start_time: String,
                end_time: Option<String>,
                duration: Option<String>,
                git_hash: String,
            }

            let items: Vec<RunJson> = runs
                .iter()
                .map(|r| RunJson {
                    id: r.id,
                    name: r.run_name.clone(),
                    start_time: r.start_time.format("%Y-%m-%dT%H:%M:%S").to_string(),
                    end_time: r
                        .end_time
                        .map(|t| t.format("%Y-%m-%dT%H:%M:%S").to_string()),
                    duration: r.end_time.map(|end| fmt_duration(r.start_time, end)),
                    git_hash: hex::encode(&r.git_commit_hash),
                })
                .collect();

            println!("{}", serde_json::to_string_pretty(&items)?);
            return Ok(());
        }

        // --- Table output ---
        let mut table = Table::new()
            .col("ID", Align::Right)
            .col("", Align::Left) // status icon
            .col_with("Name", Align::Left, 4, Some(40))
            .col("Started", Align::Left)
            .col("Duration", Align::Left)
            .col("Git Hash", Align::Left);

        for r in &runs {
            let status_icon = if r.end_time.is_some() {
                style("✔").green().to_string()
            } else {
                style("…").yellow().to_string()
            };

            let name = r.run_name.as_deref().unwrap_or("—").to_string();
            let started = fmt_relative_time(r.start_time);
            let duration = r
                .end_time
                .map(|end| fmt_duration(r.start_time, end))
                .unwrap_or_else(|| style("running").yellow().to_string());

            let hash = hex::encode(&r.git_commit_hash);
            let short_hash = hash[..hash.len().min(8)].to_string();

            table.row(vec![
                r.id.to_string(),
                status_icon,
                name,
                started,
                duration,
                short_hash,
            ]);
        }

        table.print_with_footer("run", self.limit)?;
        Ok(())
    }
}
