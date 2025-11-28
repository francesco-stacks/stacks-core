CREATE TABLE profiler_location (
  id INTEGER PRIMARY KEY NOT NULL,
  file TEXT NOT NULL,
  line INTEGER NOT NULL,
  UNIQUE(file, line)
);

CREATE TABLE profiler_span (
  id INTEGER PRIMARY KEY NOT NULL,
  name TEXT NOT NULL,
  UNIQUE(name)
);

CREATE TABLE profiler_record (
  id INTEGER PRIMARY KEY NOT NULL,
  benchmark_run_id INTEGER NOT NULL,

  -- Hierarchy
  parent_id INTEGER,
  profiler_span_id INTEGER NOT NULL,
  profiler_location_id INTEGER NOT NULL,
  child_index INTEGER NOT NULL, -- Preserves execution order for flamegraphs
  depth INTEGER NOT NULL,       -- Optimization for UI rendering

  -- Context (Mutually exclusive or both null)
  stacks_block_id INTEGER,
  stacks_tx_id INTEGER,

  -- Metrics
  wall_time_us INTEGER NOT NULL DEFAULT 0,
  cpu_time_us INTEGER NOT NULL DEFAULT 0,
  call_count INTEGER NOT NULL DEFAULT 1,

  -- Constraints
  FOREIGN KEY (benchmark_run_id) REFERENCES benchmark_run(id) ON DELETE CASCADE,
  FOREIGN KEY (parent_id) REFERENCES profiler_record(id) ON DELETE CASCADE, -- Enforce tree integrity
  FOREIGN KEY (profiler_span_id) REFERENCES profiler_span(id),
  FOREIGN KEY (profiler_location_id) REFERENCES profiler_location(id),
  FOREIGN KEY (stacks_block_id) REFERENCES stacks_block(id),
  FOREIGN KEY (stacks_tx_id) REFERENCES stacks_tx(id),

  CHECK (
    (stacks_block_id IS NOT NULL AND stacks_tx_id IS NULL) OR
    (stacks_block_id IS NULL AND stacks_tx_id IS NOT NULL) OR
    (stacks_block_id IS NULL AND stacks_tx_id IS NULL)
  )
);

-- Indexes for common access patterns
CREATE INDEX idx_prof_run_block ON profiler_record(benchmark_run_id, stacks_block_id);
CREATE INDEX idx_prof_run_tx ON profiler_record(benchmark_run_id, stacks_tx_id);
CREATE INDEX idx_prof_parent ON profiler_record(parent_id);
CREATE INDEX idx_prof_profiler_span ON profiler_record(profiler_span_id);