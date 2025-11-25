CREATE TABLE network (
  id INTEGER PRIMARY KEY NOT NULL,
  name TEXT NOT NULL UNIQUE
);

CREATE TABLE chainstate (
  id INTEGER PRIMARY KEY NOT NULL,
  network_id INTEGER NOT NULL,
  chain_id INTEGER NOT NULL,
  tip_index_hash BLOB NOT NULL,
  tip_height INTEGER NOT NULL,
  FOREIGN KEY (network_id) REFERENCES network(id),
  CHECK(length(tip_index_hash) = 32),
  UNIQUE (network_id, chain_id, tip_index_hash)
);

CREATE TABLE epoch (
    id INTEGER PRIMARY KEY NOT NULL,
    chainstate_id INTEGER NOT NULL,
    stacks_epoch_id INTEGER NOT NULL,
    network_epoch_id INTEGER NOT NULL,
    start_height INTEGER NOT NULL,
    end_height INTEGER NOT NULL,
    write_length_budget INTEGER NOT NULL,
    write_count_budget INTEGER NOT NULL,
    read_length_budget INTEGER NOT NULL,
    read_count_budget INTEGER NOT NULL,
    runtime_budget INTEGER NOT NULL,
    FOREIGN KEY (chainstate_id) REFERENCES chainstate(id),
    UNIQUE(chainstate_id, start_height, stacks_epoch_id)
);

CREATE TABLE burn_block (
    id INTEGER PRIMARY KEY NOT NULL,
    block_hash BLOB NOT NULL UNIQUE,
    height INTEGER NOT NULL,
    CHECK(length(block_hash) = 32)
);

CREATE TABLE stacks_block (
  id INTEGER PRIMARY KEY NOT NULL,
  index_hash BLOB NOT NULL UNIQUE,
  height INTEGER NOT NULL,
  parent_stacks_block_id INTEGER DEFAULT NULL,
  burn_block_id INTEGER NOT NULL,
  FOREIGN KEY (burn_block_id) REFERENCES burn_block(id),
  FOREIGN KEY (parent_stacks_block_id) REFERENCES stacks_block(id),
  CHECK(length(index_hash) = 32)
);

CREATE TABLE stacks_tx (
  id INTEGER PRIMARY KEY NOT NULL,
  stacks_block_id INTEGER NOT NULL,
  tx_hash BLOB NOT NULL UNIQUE,
  tx_type TEXT NOT NULL,
  FOREIGN KEY (stacks_block_id) REFERENCES stacks_block(id),
  UNIQUE(stacks_block_id, tx_hash),
  CHECK(length(tx_hash) = 32)
);

CREATE TABLE benchmark_run (
  id INTEGER PRIMARY KEY NOT NULL,
  run_name TEXT,
  chainstate_id INTEGER NOT NULL,
  git_commit_hash BLOB NOT NULL,
  start_time TIMESTAMP NOT NULL,
  end_time TIMESTAMP,
  args_json TEXT NOT NULL,
  FOREIGN KEY (chainstate_id) REFERENCES chainstate(id),
  CHECK(length(git_commit_hash) = 20)
);

CREATE TABLE stacks_block_stats (
  id INTEGER PRIMARY KEY NOT NULL,
  benchmark_run_id INTEGER NOT NULL,
  stacks_block_id INTEGER NOT NULL,

  -- Duration metrics (microseconds)
  total_duration_us INTEGER NOT NULL,
  setup_duration_us INTEGER NOT NULL,
  execution_duration_us INTEGER NOT NULL,
  commit_duration_us INTEGER NOT NULL,
  commit_overhead_baseline_us INTEGER NOT NULL,

  -- Clarity cost metrics (aggregated for the whole block)
  clarity_write_length INTEGER NOT NULL,
  clarity_write_count  INTEGER NOT NULL,
  clarity_read_length  INTEGER NOT NULL,
  clarity_read_count   INTEGER NOT NULL,
  clarity_runtime      INTEGER NOT NULL,

  FOREIGN KEY (benchmark_run_id) REFERENCES benchmark_run(id),
  FOREIGN KEY (stacks_block_id) REFERENCES stacks_block(id),
  UNIQUE (benchmark_run_id, stacks_block_id)
);

CREATE TABLE stacks_tx_stats (
  id INTEGER PRIMARY KEY NOT NULL,
  benchmark_run_id INTEGER NOT NULL,
  stacks_tx_id INTEGER NOT NULL,

  -- Duration metrics (microseconds)
  duration_us INTEGER NOT NULL,
  estimated_commit_impact_us INTEGER NOT NULL,

  -- Clarity cost metrics
  clarity_write_length INTEGER NOT NULL,
  clarity_write_count  INTEGER NOT NULL,
  clarity_read_length  INTEGER NOT NULL,
  clarity_read_count   INTEGER NOT NULL,
  clarity_runtime      INTEGER NOT NULL,

  FOREIGN KEY (benchmark_run_id) REFERENCES benchmark_run(id),
  FOREIGN KEY (stacks_tx_id) REFERENCES stacks_tx(id),
  UNIQUE (benchmark_run_id, stacks_tx_id)
);

-- stacks_block lookups by height
CREATE INDEX idx_stacks_block_height
  ON stacks_block(height);

-- stacks_tx lookups by parent stacks_block
CREATE INDEX idx_tx_stacks_block
  ON stacks_tx(stacks_block_id);

-- benchmark_run: helpful for lookups/filtering
CREATE INDEX idx_benchmark_run_chainstate_id
  ON benchmark_run(chainstate_id);

CREATE INDEX idx_benchmark_run_git_commit_hash
  ON benchmark_run(git_commit_hash);

CREATE INDEX idx_benchmark_run_start_time
  ON benchmark_run(start_time);

CREATE INDEX idx_benchmark_run_end_time
  ON benchmark_run(end_time);

CREATE INDEX idx_stacks_block_stats_run
  ON stacks_block_stats (benchmark_run_id);

CREATE INDEX idx_stacks_block_stats_block
  ON stacks_block_stats (stacks_block_id);

CREATE INDEX idx_stacks_tx_stats_run
  ON stacks_tx_stats (benchmark_run_id);

CREATE INDEX idx_stacks_tx_stats_tx
  ON stacks_tx_stats (stacks_tx_id);

-- Seed Network Data
INSERT INTO network (id, name) VALUES 
  (1, 'mainnet'), 
  (2, 'testnet'), 
  (3, 'regtest');
