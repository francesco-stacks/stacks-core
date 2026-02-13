use std::time::Instant;

use anyhow::Result;
use stacks_bench::db::app::{AppDb, CheckpointMode};

/// Run checkpoint + vacuum inside a "Cleaning up" multi-progress group.
///
/// Both operations are spawned onto a tokio task so the UI thread stays
/// responsive.  Errors are rendered on the spinner rather than propagated,
/// since cleanup failures are not fatal.
pub async fn run_db_cleanup(mut app_db: AppDb) -> Result<()> {
    let cleanup = cliclack::multi_progress("Cleaning up");

    let db_spinner = cleanup.add(cliclack::spinner());
    db_spinner.start("Checkpointing + vacuuming database...");
    let db_start = Instant::now();

    let db_handle = tokio::spawn(async move {
        app_db.checkpoint(CheckpointMode::Truncate).await?;
        app_db.vacuum().await?;
        Ok::<_, anyhow::Error>(())
    });

    match db_handle.await {
        Ok(Ok(())) => db_spinner.stop(fmt_success!(
            "Checkpoint + vacuum complete ({:.2}s)",
            db_start.elapsed().as_secs_f32()
        )),
        Ok(Err(e)) => db_spinner.stop(fmt_failure!(
            "Checkpoint/vacuum failed: {e} ({:.2}s)",
            db_start.elapsed().as_secs_f32()
        )),
        Err(e) => db_spinner.stop(fmt_failure!(
            "Checkpoint/vacuum task panicked: {e} ({:.2}s)",
            db_start.elapsed().as_secs_f32()
        )),
    }

    cleanup.stop();

    Ok(())
}
