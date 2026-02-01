# Profiler Explorer - AI Agent Guidelines

This document provides context for AI agents working with the Stacks Bench Profiler Explorer codebase.

## Overview

The Profiler Explorer is a web-based tool for visualizing and analyzing profiler traces from Stacks Bench benchmark runs. It displays hierarchical call trees with timing metrics (wall time, CPU time, wait time), Clarity VM costs, and key-value operation counts.

## Architecture

```
profiler-explorer/
├── app.py                    # Flask REST API backend
├── requirements.txt          # Python dependencies
├── static/                   # Vanilla JS fallback UI
│   ├── index.html
│   ├── app.js
│   └── styles.css
└── web/                      # Primary React/Vite frontend
    ├── src/
    │   ├── App.jsx           # Main component (1600+ lines)
    │   ├── main.jsx          # Entry point
    │   └── styles.css        # Tailwind + custom CSS
    ├── package.json
    ├── vite.config.js
    └── tailwind.config.js
```

### Technology Stack

- **Backend**: Python 3, Flask 3.0.2, SQLite3
- **Frontend**: React 18, Vite 5, Tailwind CSS 3.4, @svar-ui/react-grid
- **Database**: SQLite (located at `~/.stacks-bench/appdata/stacks-bench.db` or via `STACKS_BENCH_DB` env var)

## Backend API (`app.py`)

### Endpoints

| Endpoint | Purpose |
|----------|---------|
| `GET /api/runs` | List benchmark runs (latest 200) |
| `GET /api/blocks?run_id=N` | List blocks in a run |
| `GET /api/txs?run_id=N&q=prefix` | List/search transactions |
| `GET /api/tx-lookup?run_id=N&tx_hash=...` | Lookup transaction by hash |
| `GET /api/trace` | **Main endpoint** - returns profiler trace tree |

### Trace Query Modes

1. **TX Mode** (`mode=tx`): Transaction-focused trace showing all spans related to a transaction execution
   - Parameters: `run_id`, `stacks_tx_id`, `limit`

2. **RUN Mode** (`mode=run`): Block/segment scope showing span tree within specific boundaries
   - Parameters: `run_id`, `stacks_block_id`, `segment_root_id`, `min_wall_us`, `limit`

### SQL Query Patterns

The backend uses recursive CTEs to build ancestor/descendant trees:
- Walk up parent chain from seed nodes
- Walk down child chain from seed nodes
- Union both for complete trace
- Join with profiler_span, profiler_tag, stacks_tx, contract, principal tables
- Aggregate KV operations and Clarity costs

### Key Tables

- `benchmark_run` - Benchmark execution metadata
- `profiler_record` - Core trace span records
- `profiler_span` - Span definitions (name, context)
- `profiler_tag` - Optional categorization tags
- `stacks_block`, `stacks_tx` - Blockchain data
- `contract`, `contract_fn`, `principal` - Smart contract info
- `profiler_record_kv` - Key-value store operations
- `profiler_record_clarity_costs` - Clarity VM cost tracking

## Frontend (`web/src/App.jsx`)

### State Structure

```javascript
// Data selection
runs, blocks, txs, runId, stacksTxId, stacksBlockId

// Query parameters
mode ("tx" | "run"), minWallMs, segmentRootId, limit

// Display configuration
selectedColumns, numberFormatId, chainCompression, hotPathMode
heatConfig (per-metric heat map settings)

// Tree navigation
focusId, activeId, openNodes, expandedChains

// Loaded data
rows (records array), summary (status text)
```

### Data Transformation Pipeline

1. `buildTreeIndex()` - Flat records to parent-child tree
2. `pruneTree()` - Filter nodes below minWallMs threshold
3. `applyFocus()` - Zoom into selected node as root
4. `applyHotPath()` - Show only highest-cost branch
5. `applyChainCompression()` - Collapse single-child linear chains
6. `applyOpenState()` - Track expanded/collapsed state
7. Compute flame percentages for visualization

### Column System

18 columns defined in `COLUMN_DEFS`:
- Span (tree toggle + flame bar + name)
- Calls, Wall Time (Total/Avg), Busy Time (Total/Avg), Wait Time (Total)
- Samples, KV Operations
- Clarity metrics (Runtime, Read/Write counts and lengths)

Each column has: `key`, `label`, `width`, `default`, `getter`, `format`, `heatKey`, `render`

### Heat Map System

Per-metric configuration with:
- Enable/disable toggle
- Auto-calculated or custom min/max bounds
- Alpha gradient (5-27% opacity)

Heat keys: `wallTotalUs`, `selfWallUs`, `busyTotalUs`, `selfBusyUs`, `waitTotalUs`, `selfWaitUs`, `clarityRuntime`

### localStorage Keys

- `profilerColumns` - Selected visible columns
- `profilerNumberFormat` - Number format preference
- `profilerHeatConfig` - Heat map settings

## Development

### Running Locally

```bash
# Backend
cd stacks-bench/tools/profiler-explorer
pip install -r requirements.txt
python app.py  # Starts on port 8800

# Frontend (development)
cd web
npm install
npm run dev  # Starts on port 5173 with API proxy

# Frontend (production build)
npm run build  # Outputs to dist/
```

### Environment Variables

- `STACKS_BENCH_DB` - Override database path (default: `~/.stacks-bench/appdata/stacks-bench.db`)
- `PORT` - Override backend port (default: 8800)

## Common Tasks for Agents

### Adding a New Column

1. Add entry to `COLUMN_DEFS` array in `App.jsx`
2. Define `key`, `label`, `width`, `getter` function
3. Optionally add `heatKey` for heat map support
4. Add custom `render` function if needed

### Adding a New API Endpoint

1. Add route in `app.py` with `@app.route()`
2. Use parameterized SQL queries (never string interpolation)
3. Return JSON with appropriate error handling

### Modifying Tree Transformation

Tree transforms are in the pipeline between data fetch and render. Each function takes a tree and returns a modified tree. Maintain the node structure: `{ id, parentId, children, ...data }`.

### Adding Heat Map Support

1. Add `heatKey` to column definition
2. Ensure getter returns numeric microseconds
3. Heat config auto-initializes on first load

## Code Style Notes

- React functional components with hooks
- Tailwind CSS utility classes for styling
- Custom CSS variables use oklch color space
- Dark theme is the default/only theme
- Number formatting supports international locales
