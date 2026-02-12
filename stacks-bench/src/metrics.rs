use std::time::Duration;

use blockstack_lib::burnchains::Txid;
use clarity::vm::costs::ExecutionCost;
use stacks_common::types::chainstate::StacksBlockId;
use stacks_profiler::ProfileStats;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ModelSource {
    LinearRegression,
    MinAnchor,
    SingleBlock,
    Default,
}

#[derive(Debug, Clone, Copy)]
pub struct CostModel {
    pub static_overhead: Duration,
    pub time_per_byte: f64, // Seconds per byte
    pub source: ModelSource,
}

impl Default for CostModel {
    fn default() -> Self {
        Self {
            static_overhead: Duration::ZERO,
            time_per_byte: 0.0,
            source: ModelSource::Default,
        }
    }
}

impl CostModel {
    /// Compute a cost model from block metrics.
    /// Tries Linear Regression first, falls back to Min-Anchor, then Single Block.
    pub fn compute(metrics: &[BlockMetrics]) -> Self {
        // If we have enough data (e.g. > 5 blocks), skip the first few to avoid cold-cache skew.
        // We skip the first 2 blocks or 10%, whichever is larger, but cap it at 5.
        let skip_count = if metrics.len() >= 10 {
            let pct = metrics.len() / 10; // 10%
            pct.clamp(2, 5)
        } else if metrics.len() > 3 {
            1
        } else {
            0
        };

        let data = &metrics[skip_count..];

        if data.is_empty() {
            return CostModel::default();
        }

        // 1. Try Linear Regression (requires N >= 3 for stability after warmup)
        if data.len() >= 3
            && let Some(model) = Self::compute_regression(data)
        {
            return model;
        }

        // 2. Fallback: Min-Anchor Strategy
        // Find the smallest and largest blocks to draw a line between extremes
        Self::compute_min_anchor(data)
    }

    fn compute_regression(data: &[BlockMetrics]) -> Option<Self> {
        let n = data.len() as f64;
        let mut sum_x = 0.0;
        let mut sum_y = 0.0;
        let mut sum_xy = 0.0;
        let mut sum_xx = 0.0;

        for m in data {
            let x = m.total_clarity_cost.write_length as f64;
            let y = m.commit_duration.as_secs_f64();

            sum_x += x;
            sum_y += y;
            sum_xy += x * y;
            sum_xx += x * x;
        }

        let denominator = n * sum_xx - sum_x * sum_x;
        if denominator.abs() <= f64::EPSILON {
            return None;
        }

        let slope = (n * sum_xy - sum_x * sum_y) / denominator;
        let intercept = (sum_y - slope * sum_x) / n;

        // If regression yields negative values, it's invalid for our physical model
        if slope < 0.0 || intercept < 0.0 {
            return None;
        }

        Some(CostModel {
            static_overhead: Duration::from_secs_f64(intercept),
            time_per_byte: slope,
            source: ModelSource::LinearRegression,
        })
    }

    fn compute_min_anchor(data: &[BlockMetrics]) -> Self {
        let mut min_block = &data[0];
        let mut max_block = &data[0];

        for m in data {
            if m.total_clarity_cost.write_length < min_block.total_clarity_cost.write_length {
                min_block = m;
            }
            if m.total_clarity_cost.write_length > max_block.total_clarity_cost.write_length {
                max_block = m;
            }
        }

        // Case A: Single block or all blocks have same size
        if min_block.total_clarity_cost.write_length == max_block.total_clarity_cost.write_length {
            // If we only have one data point, we assume it's mostly static overhead
            // unless it's huge. This is safer than the 80/20 split because it respects
            // the actual measured time.
            return CostModel {
                static_overhead: min_block.commit_duration,
                time_per_byte: 0.0,
                source: ModelSource::SingleBlock,
            };
        }

        // Case B: Draw a line from Min (Floor) to Max (Ceiling)
        let dy = (max_block.commit_duration.as_secs_f64()
            - min_block.commit_duration.as_secs_f64())
        .max(0.0);
        let dx = (max_block.total_clarity_cost.write_length
            - min_block.total_clarity_cost.write_length) as f64;

        let slope = dy / dx;

        // The static overhead is the Min block's time minus the variable cost of its bytes
        let min_bytes = min_block.total_clarity_cost.write_length as f64;
        let intercept = (min_block.commit_duration.as_secs_f64() - (slope * min_bytes)).max(0.0);

        CostModel {
            static_overhead: Duration::from_secs_f64(intercept),
            time_per_byte: slope,
            source: ModelSource::MinAnchor,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct BlockProcessingBaseline {
    pub avg_setup_duration: Duration,
    pub avg_finalize_duration: Duration,
    pub avg_clarity_state_commit_duration: Duration,
    pub avg_advance_tip_duration: Duration,
    pub avg_index_commit_duration: Duration,
}

#[derive(Debug, Clone)]
pub struct BlockMetrics {
    pub id: StacksBlockId,
    pub synthetic_id: StacksBlockId,
    pub total_duration: Duration,
    pub setup_duration: Duration,
    pub execution_duration: Duration,
    pub commit_duration: Duration,
    pub total_clarity_cost: ExecutionCost,
    pub transactions: Vec<TransactionMetrics>,
    pub commit_overhead_baseline: Duration,
    pub total_storage_delta: i64,
    pub clarity_db_checkpoint_duration: Duration,
    /// Block-associated profiler roots
    pub profiler_roots: Vec<ProfileStats>,
}

impl BlockMetrics {
    pub fn new_default(id: StacksBlockId, synthetic_id: StacksBlockId) -> Self {
        Self {
            id,
            synthetic_id,
            total_duration: Duration::ZERO,
            setup_duration: Duration::ZERO,
            execution_duration: Duration::ZERO,
            commit_duration: Duration::ZERO,
            total_clarity_cost: ExecutionCost::ZERO,
            transactions: vec![],
            commit_overhead_baseline: Duration::ZERO,
            total_storage_delta: 0,
            clarity_db_checkpoint_duration: Duration::ZERO,
            profiler_roots: vec![],
        }
    }

    /// Apply a predictive cost model to attribute commit times.
    /// Now only computes block-level `commit_overhead_baseline`.
    pub fn apply_cost_model(&mut self, model: &CostModel) {
        let total_write_len = self.total_clarity_cost.write_length;

        let weight_static = model.static_overhead.as_secs_f64();
        let weight_variable = total_write_len as f64 * model.time_per_byte;
        let total_weight = weight_static + weight_variable;

        if total_weight <= f64::EPSILON {
            self.commit_overhead_baseline = self.commit_duration;
            return;
        }

        // Distribute actual commit duration into static vs variable (block-level only)
        let actual_seconds = self.commit_duration.as_secs_f64();
        let allocated_static = actual_seconds * (weight_static / total_weight);

        self.commit_overhead_baseline = Duration::from_secs_f64(allocated_static);
    }

    /// Apply a heuristic when no model is available.
    pub fn apply_heuristic(&mut self) {
        self.commit_overhead_baseline = self.commit_duration;
    }
}

#[derive(Debug, Clone)]
pub struct TransactionMetrics {
    pub txid: Txid,
    pub duration: Duration,
    pub cost: ExecutionCost,
    /// Tx-associated profiler roots
    pub profiler_roots: Vec<ProfileStats>,
}

#[derive(Default)]
pub struct MetricsAccumulator {
    count: u64,
    txs: u64,
    duration: Duration,
    setup: Duration,
    exec: Duration,
    commit: Duration,
    runtime: u64,
    write_len: u64,
    read_len: u64,
}

impl MetricsAccumulator {
    pub fn add(&mut self, m: &BlockMetrics) {
        self.count += 1;
        self.txs += m.transactions.len() as u64;
        self.duration += m.total_duration;
        self.setup += m.setup_duration;
        self.exec += m.execution_duration;
        self.commit += m.commit_duration;
        self.runtime += m.total_clarity_cost.runtime;
        self.write_len += m.total_clarity_cost.write_length;
        self.read_len += m.total_clarity_cost.read_length;
    }

    pub fn add_many(&mut self, ms: &[BlockMetrics]) {
        for m in ms {
            self.add(m);
        }
    }

    pub fn print_summary(&self) {
        if self.count == 0 {
            return;
        }

        println!("\n========================================");
        println!("           BENCHMARK SUMMARY            ");
        println!("========================================");
        println!("Total Blocks:       {}", self.count);
        println!("Total Transactions: {}", self.txs);
        println!("Total Duration:     {:.2?}", self.duration);
        println!("  - Setup:          {:.2?}", self.setup);
        println!("  - Execution:      {:.2?}", self.exec);
        println!("  - Commit:         {:.2?}", self.commit);
        println!("Total Clarity Runtime:  {}", self.runtime);
        println!("Total Write Length:     {} bytes", self.write_len);
        println!("Total Read Length:      {} bytes", self.read_len);

        let avg_duration = self.duration / self.count as u32;
        let avg_setup = self.setup / self.count as u32;
        let avg_exec = self.exec / self.count as u32;
        let avg_commit = self.commit / self.count as u32;
        let avg_txs = self.txs as f64 / self.count as f64;

        println!("\nAverages per Block:");
        println!("  Duration:         {:.2?}", avg_duration);
        println!("  Setup:            {:.2?}", avg_setup);
        println!("  Execution:        {:.2?}", avg_exec);
        println!("  Commit:           {:.2?}", avg_commit);
        println!("  Transactions:     {:.1}", avg_txs);
        println!("  Clarity Runtime:  {}", self.runtime / self.count);
        println!("  Write Length:     {} bytes", self.write_len / self.count);
        println!("  Read Length:      {} bytes", self.read_len / self.count);
        println!("========================================\n");
    }
}
