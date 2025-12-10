use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use stacks_common::types::chainstate::StacksBlockId;
use tokio::sync::{Semaphore, mpsc};
use tokio::task;

use crate::context::BenchContext;
use crate::db::app::AppDb;
use crate::db::node::NakamotoDb;
use crate::db::node::sortition::models::Epoch;
use crate::db::{DbOpen, ReadOnly};
use crate::{BlockTransactions, Network, ResolveEpochFromHeight as _, StacksBlockHeader};

#[derive(Debug, Default)]
struct IndexerMetrics {
    loaded_blocks: AtomicU64,
    loaded_txs: AtomicU64,
    last_loaded_height: AtomicU64,
    flushed_blocks: AtomicU64,
}

impl IndexerMetrics {
    fn record_loaded_block(&self, height: u64, tx_count: u64) {
        self.loaded_blocks.fetch_add(1, Ordering::Relaxed);
        self.loaded_txs.fetch_add(tx_count, Ordering::Relaxed);
        self.last_loaded_height.store(height, Ordering::Relaxed);
    }

    fn record_flushed_blocks(&self, count: u64) {
        self.flushed_blocks.fetch_add(count, Ordering::Relaxed);
    }
}

pub struct ChainstateIndexer<'a> {
    app_db: &'a mut AppDb,
    context: &'a BenchContext,
    batch_size: usize,
}

impl<'a> ChainstateIndexer<'a> {
    pub const DEFAULT_BATCH_SIZE: usize = 250;

    pub fn new(app_db: &'a mut AppDb, context: &'a BenchContext) -> Self {
        Self {
            app_db,
            context,
            batch_size: Self::DEFAULT_BATCH_SIZE,
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
        let (tip_id, tip_height) = self.context.chain_tip();
        let (start_height, end_height) = self.context.block_height_range()?;

        println!("Targeting block range: {start_height} to {end_height} (Tip: {tip_height})");

        let (_chainstate_model, _) = self
            .app_db
            .get_or_create_chainstate(network, chain_id, &tip_id, tip_height, epochs)
            .await?;

        // Get canonical block IDs from App DB
        let mut block_ids = self
            .app_db
            .get_chain_block_ids(&tip_id, start_height as u32, end_height as u32)
            .await?;

        let expected_count = (end_height - start_height + 1) as usize;

        if block_ids.len() != expected_count {
            println!(
                "App DB index incomplete (found {}, expected {expected_count}). Indexing from Node DB...",
                block_ids.len(),
            );

            self.run_indexing_pipeline(start_height, end_height).await?;

            println!("Checkpointing database...");
            self.app_db.checkpoint().await?;
            println!("Vacuuming database...");
            self.app_db.vacuum().await?;

            // Reload IDs
            block_ids = self
                .app_db
                .get_chain_block_ids(&tip_id, start_height as u32, end_height as u32)
                .await?;
        }

        if block_ids.is_empty() {
            return Err(anyhow!(
                "No blocks found in range {start_height} to {end_height}"
            ));
        }

        Ok(block_ids)
    }

    async fn run_indexing_pipeline(&mut self, start_height: u64, end_height: u64) -> Result<()> {
        // Channel for passing loaded blocks to the writer
        let (tx_sender, tx_receiver) =
            mpsc::channel::<Result<(StacksBlockHeader, BlockTransactions)>>(100);

        // Split borrows to allow concurrent access
        let app_db = &mut *self.app_db;
        let context = &*self.context;

        let metrics = Arc::new(IndexerMetrics {
            loaded_blocks: AtomicU64::new(0),
            loaded_txs: AtomicU64::new(0),
            last_loaded_height: AtomicU64::new(0),
            flushed_blocks: AtomicU64::new(0),
        });

        let loader_task = Self::run_loader(
            context,
            start_height,
            end_height,
            tx_sender,
            metrics.clone(),
        );
        let writer_task = Self::run_writer(app_db, tx_receiver, self.batch_size, metrics.clone());
        let reporter_task = Self::run_reporter(metrics.clone());

        // Run tasks concurrently
        // We use select! for the reporter so it stops when the others finish
        tokio::select! {
            res = async { tokio::try_join!(loader_task, writer_task) } => res.map(|_| ()),
            _ = reporter_task => Ok(()),
        }
    }

    async fn run_reporter(metrics: Arc<IndexerMetrics>) -> Result<()> {
        let start_time = Instant::now();
        let mut interval = tokio::time::interval(Duration::from_secs(5));

        loop {
            interval.tick().await;
            let elapsed = start_time.elapsed();
            let loaded = metrics.loaded_blocks.load(Ordering::Relaxed);
            let flushed = metrics.flushed_blocks.load(Ordering::Relaxed);
            let txs = metrics.loaded_txs.load(Ordering::Relaxed);
            let height = metrics.last_loaded_height.load(Ordering::Relaxed);

            let rate = if elapsed.as_secs() > 0 {
                flushed as f64 / elapsed.as_secs_f64()
            } else {
                0.0
            };

            println!(
                "  Status: Loaded {loaded} blocks (last height: {height}), {txs} txs. \
                Flushed {flushed} blocks. Rate: {rate:.1} blocks/sec"
            );
        }
    }

    async fn run_loader(
        context: &BenchContext,
        start_height: u64,
        end_height: u64,
        tx_sender: mpsc::Sender<Result<(StacksBlockHeader, BlockTransactions)>>,
        metrics: Arc<IndexerMetrics>,
    ) -> Result<()> {
        println!("  Indexing loader started");
        let chainstate_dir = context.chainstate_dir();

        // Limit concurrent DB opens/reads
        let available_parallelism = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let semaphore = Arc::new(Semaphore::new(available_parallelism));

        for block_res in context.canonical_block_stream(start_height as u32, end_height as u32) {
            // If the writer has stopped (e.g. due to error), stop spawning new tasks immediately.
            if tx_sender.is_closed() {
                break;
            }

            let header = match block_res {
                Ok(b) => b,
                Err(e) => {
                    // Propagate error to writer and stop
                    let _ = tx_sender.send(Err(e)).await;
                    break;
                }
            };

            let height = header.height;
            let epoch = context
                .resolve_stacks_epoch(header.height)
                .ok_or_else(|| anyhow!("Failed to resolve epoch for height {}", header.height))?;

            let permit = semaphore.clone().acquire_owned().await?;
            let sender = tx_sender.clone();
            let blocks_dir = chainstate_dir.blocks_dir();
            let naka_db_path = chainstate_dir.naka_db_path();
            let metrics = metrics.clone();

            task::spawn(async move {
                // Perform blocking IO in a blocking thread
                let res = task::spawn_blocking(
                    move || -> Result<(StacksBlockHeader, BlockTransactions)> {
                        // Hold permit until done
                        let _permit = permit;

                        let mut naka_db = NakamotoDb::<ReadOnly>::open(naka_db_path)
                            .context("Failed to open Nakamoto DB")?;

                        let txs =
                            BlockTransactions::load(&mut naka_db, &blocks_dir, epoch, &header)
                                .with_context(|| {
                                    format!(
                                        "Failed to load transactions for block {}",
                                        header.height
                                    )
                                })?;

                        metrics.record_loaded_block(height as u64, txs.len() as u64);

                        Ok((header, txs))
                    },
                )
                .await;

                // Handle join error and inner result
                let result = match res {
                    Ok(inner) => inner,
                    Err(e) => Err(anyhow!("Task join error: {e}")),
                };

                // Silently fail if receiver is closed (writer stopped)
                let _ = sender.send(result).await;
            });
        }

        println!("Indexing loader finished");

        Ok(())
    }

    async fn run_writer(
        app_db: &mut AppDb,
        mut tx_receiver: mpsc::Receiver<Result<(StacksBlockHeader, BlockTransactions)>>,
        batch_size: usize,
        metrics: Arc<IndexerMetrics>,
    ) -> Result<()> {
        println!("  Indexing writer started with a batch size of {batch_size}");
        let mut batch = Vec::with_capacity(batch_size);

        while let Some(res) = tx_receiver.recv().await {
            let (block, txs) = res?;
            batch.push((block, txs));

            if batch.len() >= batch_size {
                let count = batch.len() as u64;
                let headers: Vec<_> = batch.iter().map(|(b, _)| b.clone()).collect();

                app_db.stage_blocks(headers).await?;
                app_db.stage_transactions(batch.drain(..)).await?;

                metrics.record_flushed_blocks(count);
            }
        }

        // Flush remaining
        if !batch.is_empty() {
            let headers: Vec<_> = batch.iter().map(|(b, _)| b.clone()).collect();
            app_db.stage_blocks(headers).await?;
            app_db.stage_transactions(batch).await?;
        }

        println!("  Last block received - merging staged data. This may take a few moments...");
        app_db.merge_staging().await?;
        println!("  Merge complete!");

        println!("Indexing writer finished");
        Ok(())
    }
}
