-- ==========================================
-- Enum table for Bitcoin (and implicitly Stacks) networks.
-- ==========================================
CREATE TABLE network (
  id INTEGER PRIMARY KEY NOT NULL,
  name TEXT NOT NULL UNIQUE
);

-- Default networks
INSERT INTO network (id, name) VALUES 
  (1, 'mainnet'), 
  (2, 'testnet'), 
  (3, 'regtest');

-- ==========================================
-- Chainstate snapshot tracking, effectively unique per (network, chain_id, 
-- tip, epochs).
-- ==========================================
CREATE TABLE chainstate (
  id INTEGER PRIMARY KEY NOT NULL,
  network_id INTEGER NOT NULL,
  chain_id INTEGER NOT NULL,
  tip_index_hash BLOB NOT NULL,
  tip_height INTEGER NOT NULL,
  epochs_hash BLOB NOT NULL,
  FOREIGN KEY (network_id) REFERENCES network(id),
  CHECK(length(tip_index_hash) = 32),
  CHECK(length(epochs_hash) = 32),
  UNIQUE (network_id, chain_id, tip_index_hash, epochs_hash)
);

-- ==========================================
-- Dimension for epochs, unique per chainstate. Pulled from the Stacks
-- sortition database.
-- ==========================================
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
    UNIQUE(chainstate_id, stacks_epoch_id)
);

-- ==========================================
-- Dimension for Stacks transaction types which have been seen.
-- ==========================================
CREATE TABLE stacks_tx_type (
    id INTEGER PRIMARY KEY NOT NULL,
    name TEXT NOT NULL UNIQUE
);

CREATE TABLE _staged_stacks_tx_type (
    name TEXT NOT NULL
);

-- ==========================================
-- Dimension for Stacks principals which have been seen.
-- ==========================================
CREATE TABLE principal (
    id INTEGER PRIMARY KEY NOT NULL,
    address TEXT NOT NULL UNIQUE
);

-- ==========================================
-- Staging table for Stacks principals during bulk import.
-- ==========================================
CREATE TABLE _staged_principal (
    address TEXT NOT NULL
);

-- ==========================================
-- Dimension for Stacks contracts which have been seen.
-- ==========================================
CREATE TABLE contract (
    id INTEGER PRIMARY KEY NOT NULL,
    issuer_principal_id INTEGER NOT NULL,
    name TEXT NOT NULL,
    FOREIGN KEY (issuer_principal_id) REFERENCES principal(id),
    UNIQUE(issuer_principal_id, name)
);

CREATE INDEX idx_contract_principal ON contract(issuer_principal_id);
CREATE INDEX idx_contract_name ON contract(name);

-- ==========================================
-- Staging table for Stacks contracts during bulk import.
-- ==========================================
CREATE TABLE _staged_contract (
    issuer_address TEXT NOT NULL,
    name TEXT NOT NULL
);

-- ==========================================
-- Dimension for burn (Bitcoin) blocks. Not linked to any specific
-- chainstate as they are cryptographically deterministic.
-- ==========================================
CREATE TABLE burn_block (
    id INTEGER PRIMARY KEY NOT NULL,
    block_hash BLOB NOT NULL UNIQUE,
    block_hash_hex TEXT GENERATED ALWAYS AS (LOWER(HEX(block_hash))) STORED,
    height INTEGER NOT NULL,
    CHECK(height >= 0),
    CHECK(length(block_hash) = 32)
);

CREATE INDEX idx_burn_block_height ON burn_block(height);
CREATE INDEX idx_burn_block_block_hash_hex ON burn_block(block_hash_hex);

-- ==========================================
-- Dimension for Stacks blocks. Not linked to any specific chainstate as they
-- are cryptographically deterministic.
-- ==========================================
CREATE TABLE stacks_block (
  id INTEGER PRIMARY KEY NOT NULL,
  index_hash BLOB NOT NULL UNIQUE,
  index_hash_hex TEXT GENERATED ALWAYS AS (LOWER(HEX(index_hash))) STORED,
  block_hash BLOB NOT NULL,
  block_hash_hex TEXT GENERATED ALWAYS AS (LOWER(HEX(block_hash))) STORED,
  height INTEGER NOT NULL,
  parent_stacks_block_id INTEGER DEFAULT NULL,
  burn_block_id INTEGER NOT NULL,
  FOREIGN KEY (burn_block_id) REFERENCES burn_block(id),
  FOREIGN KEY (parent_stacks_block_id) REFERENCES stacks_block(id),
  CHECK(height >= 0),
  CHECK(length(index_hash) = 32)
);

CREATE INDEX idx_stacks_block_height ON stacks_block(height);
CREATE INDEX idx_stacks_block_index_hash_hex ON stacks_block(index_hash_hex);
CREATE INDEX idx_stacks_block_block_hash_hex ON stacks_block(block_hash_hex);

-- ==========================================
-- Staging table for Stacks blocks during bulk import.
-- ==========================================
CREATE TABLE _staged_stacks_block (
    index_hash BLOB NOT NULL,
    block_hash BLOB NOT NULL,
    parent_index_hash BLOB NOT NULL,
    height INTEGER NOT NULL,
    burn_block_hash BLOB NOT NULL,
    burn_block_height INTEGER NOT NULL
);

-- ==========================================
-- Dimension for Stacks transactions. Not linked to any specific chainstate as 
-- they are cryptographically deterministic.
-- ==========================================
CREATE TABLE stacks_tx (
  id INTEGER PRIMARY KEY NOT NULL,
  stacks_block_id INTEGER NOT NULL,
  tx_hash BLOB NOT NULL UNIQUE,
  tx_hash_hex TEXT GENERATED ALWAYS AS (LOWER(HEX(tx_hash))) STORED,
  stacks_tx_type_id INTEGER NOT NULL,
  caller_principal_id INTEGER NOT NULL,
  contract_id INTEGER,
  FOREIGN KEY (stacks_block_id) REFERENCES stacks_block(id),
  FOREIGN KEY (stacks_tx_type_id) REFERENCES stacks_tx_type(id),
  FOREIGN KEY (caller_principal_id) REFERENCES principal(id),
  FOREIGN KEY (contract_id) REFERENCES contract(id),
  UNIQUE(stacks_block_id, tx_hash),
  CHECK(length(tx_hash) = 32)
);

CREATE INDEX idx_tx_tx_hash_hex ON stacks_tx(tx_hash_hex);
CREATE INDEX idx_tx_caller_principal ON stacks_tx(caller_principal_id);
CREATE INDEX idx_tx_contract ON stacks_tx(contract_id);

-- ==========================================
-- Staging table for Stacks transactions during bulk import.
-- ==========================================
CREATE TABLE _staged_stacks_tx (
    block_index_hash BLOB NOT NULL,
    tx_hash BLOB NOT NULL,
    tx_type TEXT NOT NULL,
    caller_address TEXT NOT NULL,
    contract_issuer_address TEXT,
    contract_name TEXT
);

-- ==========================================
-- Dimension for benchmark runs.
-- ==========================================
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

-- ==========================================
-- Fact table for benchmark statistics per Stacks block.
-- ==========================================
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

  -- Total storage delta (in bytes) resulting from block processing
  total_storage_delta INTEGER NOT NULL,

  FOREIGN KEY (benchmark_run_id) REFERENCES benchmark_run(id),
  FOREIGN KEY (stacks_block_id) REFERENCES stacks_block(id),
  UNIQUE (benchmark_run_id, stacks_block_id)
);

CREATE INDEX idx_stacks_block_stats_block ON stacks_block_stats (stacks_block_id);

-- ==========================================
-- Fact table for benchmark statistics per Stacks transaction.
-- ==========================================
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

CREATE INDEX idx_stacks_tx_stats_tx ON stacks_tx_stats (stacks_tx_id);

-- ==========================================
-- Dimension table for profiler locations (file + line).
-- ==========================================
CREATE TABLE profiler_location (
  id INTEGER PRIMARY KEY NOT NULL,
  file TEXT NOT NULL,
  line INTEGER NOT NULL,
  UNIQUE(file, line)
);

-- ==========================================
-- Dimension table for profiler spans (named code regions).
-- ==========================================
CREATE TABLE profiler_span (
  id INTEGER PRIMARY KEY NOT NULL,
  context TEXT,
  name TEXT NOT NULL,
  UNIQUE(context, name)
);

-- ==========================================
-- Dimension table for profiler records (hierarchical timing data, per span and parent).
-- ==========================================
CREATE TABLE profiler_record (
  id INTEGER PRIMARY KEY NOT NULL,
  benchmark_run_id INTEGER NOT NULL,

  -- Hierarchy
  parent_id INTEGER,
  profiler_span_id INTEGER NOT NULL,
  profiler_location_id INTEGER NOT NULL,
  child_index INTEGER NOT NULL, -- Preserves execution order for flamegraphs
  depth INTEGER NOT NULL,       -- Optimization for UI rendering

  -- Context
  stacks_block_id INTEGER,
  stacks_tx_id INTEGER,

  -- Metrics
  wall_time_us INTEGER NOT NULL,
  cpu_time_us INTEGER NOT NULL,
  -- Exclusive wall time (wall_time - sum(children.wall_time))
  self_wall_time_us INTEGER NOT NULL,
  -- Exclusive CPU time (cpu_time - sum(children.cpu_time))
  self_cpu_time_us INTEGER NOT NULL,
  call_count INTEGER NOT NULL,

  -- Constraints
  FOREIGN KEY (benchmark_run_id) REFERENCES benchmark_run(id) ON DELETE CASCADE,
  FOREIGN KEY (parent_id) REFERENCES profiler_record(id) ON DELETE CASCADE,
  FOREIGN KEY (profiler_span_id) REFERENCES profiler_span(id),
  FOREIGN KEY (profiler_location_id) REFERENCES profiler_location(id),
  FOREIGN KEY (stacks_block_id) REFERENCES stacks_block(id),
  FOREIGN KEY (stacks_tx_id) REFERENCES stacks_tx(id)
);

CREATE INDEX idx_prof_run_block 
  ON profiler_record(benchmark_run_id, stacks_block_id)
  WHERE stacks_block_id IS NOT NULL;
CREATE INDEX idx_prof_run_tx 
  ON profiler_record(benchmark_run_id, stacks_tx_id)
  WHERE stacks_tx_id IS NOT NULL;
CREATE INDEX idx_prof_parent ON profiler_record(parent_id);
CREATE INDEX idx_prof_profiler_span ON profiler_record(profiler_span_id);

-- ==========================================
-- Cache table for chain tip lookups to speed up ancestor queries (e.g. when
-- determining the Stacks block at a given height for a specific tip).
-- ==========================================
CREATE TABLE chain_tip_cache (
  tip_index_hash BLOB NOT NULL,
  height BIGINT NOT NULL,
  index_hash BLOB NOT NULL,
  PRIMARY KEY (tip_index_hash, height),
  CHECK(LENGTH(tip_index_hash) = 32),
  CHECK(LENGTH(index_hash) = 32),
  CHECK(height >= 0)
);
