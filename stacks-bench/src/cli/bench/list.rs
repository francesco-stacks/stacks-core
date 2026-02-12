use std::fmt::Write as _;

use anyhow::{Result, bail};
use chrono::{NaiveDateTime, Utc};
use console::style;

use crate::cli::common::CliContext;

/// Parse a human-friendly duration string like `10m`, `2h`, `1d6h`, `1d12h30m`.
///
/// Supported units: `d` (days), `h` (hours), `m` (minutes).
fn parse_since(s: &str) -> Result<chrono::Duration> {
    let mut total_minutes: i64 = 0;
    let mut buf = String::new();
    for ch in s.chars() {
        match ch {
            'd' => {
                let n: i64 = buf
                    .parse()
                    .map_err(|_| anyhow::anyhow!("invalid number before 'd': '{buf}'"))?;
                total_minutes += n * 24 * 60;
                buf.clear();
            }
            'h' => {
                let n: i64 = buf
                    .parse()
                    .map_err(|_| anyhow::anyhow!("invalid number before 'h': '{buf}'"))?;
                total_minutes += n * 60;
                buf.clear();
            }
            'm' => {
                let n: i64 = buf
                    .parse()
                    .map_err(|_| anyhow::anyhow!("invalid number before 'm': '{buf}'"))?;
                total_minutes += n;
                buf.clear();
            }
            c if c.is_ascii_digit() => buf.push(c),
            _ => bail!("unexpected character '{}' in --since value", ch),
        }
    }
    // Bare number without unit → treat as minutes
    if !buf.is_empty() {
        let n: i64 = buf
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid trailing number in --since: '{buf}'"))?;
        total_minutes += n;
    }
    if total_minutes == 0 {
        bail!("--since value resolves to zero duration");
    }
    Ok(chrono::Duration::minutes(total_minutes))
}

/// Format a `NaiveDateTime` as a human‐friendly relative string like "2h 13m ago".
fn relative_time(ts: NaiveDateTime) -> String {
    let now = Utc::now().naive_utc();
    let delta = now.signed_duration_since(ts);
    if delta.num_seconds() < 60 {
        return "just now".to_string();
    }
    let mut parts = Vec::new();
    let days = delta.num_days();
    let hours = delta.num_hours() % 24;
    let minutes = delta.num_minutes() % 60;
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes}m"));
    }
    if parts.is_empty() {
        "just now".to_string()
    } else {
        format!("{} ago", parts.join(" "))
    }
}

/// Format a duration between two timestamps as a compact string.
fn format_duration(start: NaiveDateTime, end: NaiveDateTime) -> String {
    let secs = end.signed_duration_since(start).num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

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
                    duration: r.end_time.map(|end| format_duration(r.start_time, end)),
                    git_hash: hex::encode(&r.git_commit_hash),
                })
                .collect();

            println!("{}", serde_json::to_string_pretty(&items)?);
            return Ok(());
        }

        // --- Table output ---
        //
        // Columns: ID | Status | Name | Started | Duration | Git Hash
        //
        // Compute column widths dynamically.
        let id_w = runs
            .iter()
            .map(|r| r.id.to_string().len())
            .max()
            .unwrap_or(2)
            .max(2);
        let name_w = runs
            .iter()
            .map(|r| r.run_name.as_deref().unwrap_or("—").len())
            .max()
            .unwrap_or(4)
            .clamp(4, 40);

        // Header — pad plain strings first, then apply styles so ANSI escapes
        // don't throw off column alignment.
        let h_id = format!("{:<id_w$}", "ID", id_w = id_w);
        let h_name = format!("{:<name_w$}", "Name", name_w = name_w);
        let h_started = format!("{:<16}", "Started");
        let h_dur = format!("{:<10}", "Duration");
        let h_hash = "Git Hash";
        let header = format!(
            "  {}  {}  {}  {}  {}  {}",
            style(h_id).bold().underlined(),
            style("  ").underlined(),
            style(h_name).bold().underlined(),
            style(h_started).bold().underlined(),
            style(h_dur).bold().underlined(),
            style(h_hash).bold().underlined(),
        );
        println!();
        println!("{header}");

        for r in &runs {
            let status_icon = if r.end_time.is_some() {
                style("✔").green().to_string()
            } else {
                style("…").yellow().to_string()
            };

            let name_str = r.run_name.as_deref().unwrap_or("—");
            let name_display = if name_str.len() > name_w {
                format!("{}…", &name_str[..name_w - 1])
            } else {
                name_str.to_string()
            };

            let started = relative_time(r.start_time);
            let duration = r
                .end_time
                .map(|end| format!("{:<10}", format_duration(r.start_time, end)))
                .unwrap_or_else(|| format!("{:<10}", style("running").yellow()));

            let hash = hex::encode(&r.git_commit_hash);
            let short_hash = &hash[..hash.len().min(8)];

            let mut line = String::new();
            write!(
                line,
                "  {id:>id_w$}  {status_icon} {name_display:<name_w$}  {started:<16}  {duration}  {short_hash}",
                id = r.id,
                id_w = id_w,
                name_display = name_display,
                name_w = name_w,
                status_icon = status_icon,
                started = started,
                duration = duration,
                short_hash = short_hash,
            )
            .unwrap();

            println!("{line}");
        }

        println!();
        let showing = runs.len();
        let suffix = if showing == self.limit {
            format!(" (limit {}; use --limit to show more)", self.limit)
        } else {
            String::new()
        };
        cliclack::log::info(format!(
            "{showing} run{s}{suffix}",
            s = if showing == 1 { "" } else { "s" }
        ))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_since_basic() {
        assert_eq!(parse_since("10m").unwrap().num_minutes(), 10);
        assert_eq!(parse_since("2h").unwrap().num_minutes(), 120);
        assert_eq!(parse_since("1d").unwrap().num_minutes(), 1440);
        assert_eq!(parse_since("1d6h").unwrap().num_minutes(), 1800);
        assert_eq!(parse_since("1d12h30m").unwrap().num_minutes(), 2190);
    }

    #[test]
    fn parse_since_bare_number_is_minutes() {
        assert_eq!(parse_since("30").unwrap().num_minutes(), 30);
    }

    #[test]
    fn parse_since_errors() {
        assert!(parse_since("").is_err());
        assert!(parse_since("0m").is_err());
        assert!(parse_since("abc").is_err());
    }
}
