use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use blockstack_lib::chainstate::stacks::StacksTransaction;
use futures::StreamExt;
use stacks_common::types::chainstate::StacksBlockId;
use tokio::sync::mpsc;
use tokio::task;

use crate::context::BenchContext;
use crate::db::app::AppDb;
use crate::db::node::sortition::models::Epoch;
use crate::{Network, StacksBlockHeader, StacksBlockLoader};

#[derive(Debug, Default)]
struct IndexerMetrics {
    loaded_blocks: AtomicUsize,
    loaded_txs: AtomicUsize,
    last_loaded_height: AtomicU64,
    flushed_blocks: AtomicUsize,
    flushed_txs: AtomicUsize,
}

impl IndexerMetrics {
    fn record_loaded_block(&self, height: u64, tx_count: usize) {
        self.loaded_blocks.fetch_add(1, Ordering::Relaxed);
        self.loaded_txs.fetch_add(tx_count, Ordering::Relaxed);
        self.last_loaded_height.store(height, Ordering::Relaxed);
    }

    fn record_flush(&self, block_count: usize, tx_count: usize) {
        self.flushed_blocks
            .fetch_add(block_count, Ordering::Relaxed);
        self.flushed_txs.fetch_add(tx_count, Ordering::Relaxed);
    }
}

pub struct ChainstateIndexer<'a> {
    app_db: &'a mut AppDb,
    context: &'a mut BenchContext,
    batch_size: usize,
    merge_threshold: usize,
    channel_buffer_size: usize,
}

impl<'a> ChainstateIndexer<'a> {
    pub const DEFAULT_BATCH_SIZE: usize = 1_000;
    pub const DEFAULT_MERGE_THRESHOLD: usize = 100_000;
    pub const DEFAULT_CHANNEL_BUFFER_SIZE: usize = 5_000;

    pub fn new(app_db: &'a mut AppDb, context: &'a mut BenchContext) -> Self {
        Self {
            app_db,
            context,
            batch_size: Self::DEFAULT_BATCH_SIZE,
            merge_threshold: Self::DEFAULT_MERGE_THRESHOLD,
            channel_buffer_size: Self::DEFAULT_CHANNEL_BUFFER_SIZE,
        }
    }

    pub fn set_batch_size(&mut self, batch_size: usize) {
        self.batch_size = batch_size;
    }

    pub async fn index_chainstate(
        &mut self,
        network: Network,
        chain_id: u32,
        epochs: &[Epoch],
    ) -> Result<Vec<StacksBlockId>> {
        let chain_tip = self.context.chain_tip().clone();
        let (start_height, end_height) = self.context.block_height_range()?;
        let end_block_id = self.context.end_block().id.clone();

        // Index one extra block so the first replayed block can resolve its parent in AppDb::get_block().
        let index_start_height = if start_height > 0 {
            start_height - 1
        } else {
            start_height
        };

        println!("Targeting block range: {start_height} to {end_height} (Tip: {chain_tip})");

        let (_chainstate_model, _) = self
            .app_db
            .get_or_create_chainstate(network, chain_id, &chain_tip, epochs)
            .await?;

        let expected_indexed_count = (end_height - index_start_height + 1) as usize;

        // Query the indexed segment (may include start-1).
        let mut indexed_ids = self
            .app_db
            .get_chain_block_ids(&end_block_id, index_start_height, end_height)
            .await?;

        if indexed_ids.len() != expected_indexed_count {
            println!(
                "App DB index incomplete (found {}, expected {expected_indexed_count}). Indexing from Node DB...",
                indexed_ids.len(),
            );

            self.run_indexing_pipeline(index_start_height, end_height)
                .await?;

            println!("Checkpointing database...");
            self.app_db.checkpoint(true).await?;
            println!("Vacuuming database...");
            self.app_db.vacuum().await?;

            // Reload indexed IDs after indexing/merge.
            indexed_ids = self
                .app_db
                .get_chain_block_ids(&end_block_id, index_start_height, end_height)
                .await?;
        }

        if indexed_ids.is_empty() {
            return Err(anyhow!(
                "No blocks found in indexed range {index_start_height} to {end_height}"
            ));
        }

        if indexed_ids.len() != expected_indexed_count {
            return Err(anyhow!(
                "Index still incomplete after indexing (found {}, expected {expected_indexed_count})",
                indexed_ids.len(),
            ));
        }

        // Return only the requested range [start_height..=end_height].
        let block_ids: Vec<StacksBlockId> = if start_height > 0 {
            indexed_ids.into_iter().skip(1).collect()
        } else {
            indexed_ids
        };

        let expected_count = (end_height - start_height + 1) as usize;
        if block_ids.len() != expected_count {
            return Err(anyhow!(
                "Unexpected block id count (found {}, expected {expected_count})",
                block_ids.len(),
            ));
        }

        Ok(block_ids)
    }

    async fn run_indexing_pipeline(&mut self, start_height: u64, end_height: u64) -> Result<()> {
        // Channel for passing loaded blocks to the writer
        let (tx_sender, tx_receiver) =
            mpsc::channel::<Result<(StacksBlockHeader, Vec<StacksTransaction>)>>(100);

        let metrics = Arc::new(IndexerMetrics::default());

        let loader_task = Self::run_loader(
            &mut self.context,
            start_height,
            end_height,
            self.channel_buffer_size,
            tx_sender,
            metrics.clone(),
        );

        let writer_task = Self::run_writer(
            &mut self.app_db,
            tx_receiver,
            self.batch_size,
            self.merge_threshold,
            metrics.clone(),
        );

        // Spawn the reporter task so it runs independently of the loader/writer loop.
        // This prevents the reporter from stalling if the writer performs a long blocking operation (like a merge/checkpoint).
        let reporter_handle = task::spawn(run_reporter(metrics.clone(), start_height, end_height));

        // Run loader and writer concurrently on the current task
        let result = tokio::try_join!(loader_task, writer_task);

        // Abort the reporter once the work is done (or if it failed)
        reporter_handle.abort();

        result.map(|_| ())
    }

    async fn run_loader(
        context: &mut BenchContext,
        start_height: u64,
        end_height: u64,
        channel_buffer_size: usize,
        tx_sender: mpsc::Sender<Result<(StacksBlockHeader, Vec<StacksTransaction>)>>,
        metrics: Arc<IndexerMetrics>,
    ) -> Result<()> {
        println!("  Indexing loader started");
        let chainstate_dir = context.chainstate_dir();
        let blocks_dir = chainstate_dir.blocks_dir();

        let nakamoto_db = context.open_nakamoto_db_for_read().await?;
        let min_naka_height = nakamoto_db
            .get_min_block_height()
            .await?
            .unwrap_or(u64::MAX);
        println!("Nakamoto blocks DB first block height: {min_naka_height:?}");

        // Limit concurrent DB opens/reads
        let available_parallelism = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        let worker_count = available_parallelism * 2;

        // Create a channel to distribute work to the workers.
        let (work_tx, work_rx) = mpsc::channel::<StacksBlockHeader>(channel_buffer_size);

        // Wrap the receiver in a tokio Mutex to share it among async workers.
        let work_rx = Arc::new(tokio::sync::Mutex::new(work_rx));

        let mut handles = Vec::new();

        // Spawn persistent workers
        for _ in 0..worker_count {
            let rx = work_rx.clone();
            let tx = tx_sender.clone();
            let b_dir = blocks_dir.clone();
            let metrics = metrics.clone();
            // One read-only instance of the naka db per worker
            let mut naka_db = nakamoto_db.clone();

            handles.push(tokio::spawn(async move {
                loop {
                    // Fetch next job
                    let header = {
                        // Lock the shared receiver
                        let mut locked_rx = rx.lock().await;
                        match locked_rx.recv().await {
                            Some(h) => h,
                            None => break, // Channel closed, work finished
                        }
                    };

                    let mut loader = StacksBlockLoader::new(&b_dir, &mut naka_db, min_naka_height);
                    let load_res = loader.load_block(&header).await.with_context(|| {
                        format!("Failed to load transactions for block {}", header.height)
                    });

                    match load_res {
                        Ok(block) => {
                            metrics.record_loaded_block(header.height, block.transactions().len());
                            if tx
                                .send(Ok((header, block.into_transactions_vec())))
                                .await
                                .is_err()
                            {
                                break; // Receiver dropped
                            }
                        }
                        Err(e) => {
                            // Send the error to the writer/main thread so it can abort gracefully
                            let _ = tx.send(Err(e)).await;
                            break; // Worker exits after reporting error
                        }
                    }
                }
                Ok::<(), anyhow::Error>(())
            }));
        }

        // Feed the workers from the main stream
        let mut stream = context
            .canonical_block_stream(start_height as u32, end_height as u32)
            .await;
        while let Some(block_res) = stream.next().await {
            // If the writer has stopped (e.g. due to error), stop spawning new tasks immediately.
            if tx_sender.is_closed() {
                break;
            }

            match block_res {
                Ok(header) => {
                    // Use .await here to handle backpressure without blocking the runtime
                    if work_tx.send(header).await.is_err() {
                        break; // Workers died
                    }
                }
                Err(e) => {
                    // Propagate error to writer and stop
                    let _ = tx_sender.send(Err(e)).await;
                    break;
                }
            }
        }

        // Close the work channel to signal workers to exit
        drop(work_tx);

        // Wait for all workers to finish cleanly
        for handle in handles {
            match handle.await {
                Ok(res) => res?,
                Err(e) => return Err(anyhow!("Worker task panicked: {}", e)),
            }
        }
        println!("Indexing loader finished");

        Ok(())
    }

    async fn run_writer(
        app_db: &mut AppDb,
        mut tx_receiver: mpsc::Receiver<Result<(StacksBlockHeader, Vec<StacksTransaction>)>>,
        batch_size: usize,
        merge_threshold: usize,
        metrics: Arc<IndexerMetrics>,
    ) -> Result<()> {
        println!("  Indexing writer started with a batch size of {batch_size}");
        let mut batch = Vec::with_capacity(batch_size);
        let mut txs_since_last_merge = 0;

        while let Some(res) = tx_receiver.recv().await {
            let (header, transactions) = res?;
            batch.push((header, transactions));

            // We still batch writes by block count to keep memory usage predictable
            if batch.len() >= batch_size {
                let block_count = batch.len();
                let headers: Vec<_> = batch.iter().map(|(h, _)| h.clone()).collect();

                // Calculate total txs in this batch for the merge threshold
                let tx_count: usize = batch.iter().map(|(_, txs)| txs.len()).sum();
                txs_since_last_merge += tx_count;

                app_db.stage_blocks(headers).await?;
                app_db.stage_transactions(batch.drain(..)).await?;

                metrics.record_flush(block_count, tx_count);

                // Check if we should merge staging data based on TRANSACTION count
                if txs_since_last_merge >= merge_threshold {
                    println!(
                        "  Merge threshold reached ({txs_since_last_merge} txs). Merging staging data..."
                    );
                    let start = Instant::now();
                    app_db.merge_staging().await?;

                    // Checkpoint to keep WAL size manageable since auto-checkpoint is disabled
                    app_db.checkpoint(false).await?;

                    println!(
                        "  Incremental merge & checkpoint complete in {:.2?}",
                        start.elapsed()
                    );
                    txs_since_last_merge = 0;
                }
            }
        }

        // Flush remaining
        if !batch.is_empty() {
            let headers: Vec<_> = batch.iter().map(|(b, _)| b.clone()).collect();
            app_db.stage_blocks(headers).await?;
            app_db.stage_transactions(batch).await?;
        }

        println!("  Last block received - performing final staging data merge");
        let start = Instant::now();
        app_db.merge_staging().await?;
        println!("  Final merge complete in {:.2?}", start.elapsed());

        println!("Indexing writer finished");
        Ok(())
    }
}

async fn run_reporter(
    metrics: Arc<IndexerMetrics>,
    start_height: u64,
    end_height: u64,
) -> Result<()> {
    let total_blocks = (end_height.saturating_sub(start_height) + 1) as usize;
    let mut interval = tokio::time::interval(Duration::from_secs(5));

    let mut last_flushed_blocks = 0;
    let mut last_loaded_blocks = 0;
    let mut last_loaded_txs = 0;
    let mut last_flushed_txs = 0;
    let mut last_time = Instant::now();

    loop {
        interval.tick().await;
        let now = Instant::now();
        let elapsed = now.duration_since(last_time).as_secs_f64();

        let current_loaded_blocks = metrics.loaded_blocks.load(Ordering::Relaxed);
        let current_flushed_blocks = metrics.flushed_blocks.load(Ordering::Relaxed);
        let current_loaded_txs = metrics.loaded_txs.load(Ordering::Relaxed);
        let current_flushed_txs = metrics.flushed_txs.load(Ordering::Relaxed);
        let current_height = metrics.last_loaded_height.load(Ordering::Relaxed);

        let delta_loaded_blocks = current_loaded_blocks.saturating_sub(last_loaded_blocks);
        let delta_flushed_blocks = current_flushed_blocks.saturating_sub(last_flushed_blocks);
        let delta_loaded_txs = current_loaded_txs.saturating_sub(last_loaded_txs);
        let delta_flushed_txs = current_flushed_txs.saturating_sub(last_flushed_txs);

        let rate_loaded_blocks = if elapsed > 0.0 {
            delta_loaded_blocks as f64 / elapsed
        } else {
            0.0
        };

        let rate_flushed_blocks = if elapsed > 0.0 {
            delta_flushed_blocks as f64 / elapsed
        } else {
            0.0
        };

        let rate_loaded_txs = if elapsed > 0.0 {
            delta_loaded_txs as f64 / elapsed
        } else {
            0.0
        };

        let rate_flushed_txs = if elapsed > 0.0 {
            delta_flushed_txs as f64 / elapsed
        } else {
            0.0
        };

        let progress = if total_blocks > 0 {
            (current_flushed_blocks as f64 / total_blocks as f64) * 100.0
        } else {
            0.0
        };

        println!(
            "  Status: {progress:>5.1}% | Height: {current_height:<7} | \
            Blocks: +{delta_loaded_blocks:<4} ({rate_loaded_blocks:>5.1}/s) -> +{delta_flushed_blocks:<4} ({rate_flushed_blocks:>5.1}/s) | \
            Txs: +{delta_loaded_txs:<5} ({rate_loaded_txs:>6.1}/s) -> +{delta_flushed_txs:<5} ({rate_flushed_txs:>6.1}/s)"
        );

        last_loaded_blocks = current_loaded_blocks;
        last_flushed_blocks = current_flushed_blocks;
        last_loaded_txs = current_loaded_txs;
        last_flushed_txs = current_flushed_txs;
        last_time = now;

        if current_flushed_blocks >= total_blocks {
            break;
        }
    }
    Ok(())
}
