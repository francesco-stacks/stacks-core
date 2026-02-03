import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Grid } from "@svar-ui/react-grid";
import { WillowDark } from "@svar-ui/react-core";
import {
  Loader2,
  Filter,
  X,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Combobox } from "@/components/ui/combobox";
import {
  getTransactions,
  getTransactionsAutocomplete,
  getTransactionsMaxes,
} from "@/lib/api.ts";
import { TxContextHeatCell, TxContextHeatHeaderCell, TxContextActionCell } from "./HeatCells";
import { TransactionsGridProvider } from "../contexts/TransactionsGridContext";
import { HEAT_COLOR_OPTIONS } from "../profilerConfig.ts";

// Buffer size - how many rows to fetch beyond visible area
const FETCH_BUFFER = 100;
// Minimum rows to fetch per request
const MIN_FETCH_SIZE = 200;

function truncateHash(hash, len = 12) {
  if (!hash) return "-";
  if (hash.length <= len * 2) return hash;
  return `${hash.slice(0, len)}…${hash.slice(-len)}`;
}

// Cell component for transaction hash
function TxHashCell({ row }) {
  return (
    <span className="tx-hash-text numeric-text" title={row.tx_hash_hex}>
      {row.tx_hash_hex || "-"}
    </span>
  );
}

const DEFAULT_HEAT_MAXES = {
  duration_ms: 0,
  clarity_runtime: 0,
  clarity_read_count: 0,
  clarity_read_length: 0,
  clarity_write_count: 0,
  clarity_write_length: 0,
};

export default function TransactionsTab({ runId, onViewTrace, numberFormat }) {
  // Grid data - sparse array indexed by row position
  const [dataCache, setDataCache] = useState([]);
  // Current visible range for the grid (svar-ui expects only visible slice in data prop)
  const [visibleRange, setVisibleRange] = useState({ start: 0, end: MIN_FETCH_SIZE });
  const [total, setTotal] = useState(0);
  const [isLoading, setIsLoading] = useState(false);
  const [error, setError] = useState(null);
  const [sortBy, setSortBy] = useState("duration_ms");
  const [sortDir, setSortDir] = useState("desc");
  const [filters, setFilters] = useState({
    principal: [],
    contract: [],
    contractFn: [],
    minDurationMs: "",
  });
  const [filterSearch, setFilterSearch] = useState({
    principal: "",
    contract: "",
    contractFn: "",
  });
  const [autocomplete, setAutocomplete] = useState({
    principal: [],
    contract: [],
    contractFn: [],
  });
  const [autocompleteLoading, setAutocompleteLoading] = useState({
    principal: false,
    contract: false,
    contractFn: false,
  });
  const autocompleteAbortRef = useRef({});
  const [showFilters, setShowFilters] = useState(true);
  const abortControllerRef = useRef(null);
  const fetchedRangesRef = useRef([]);
  const gridApiRef = useRef(null);
  // Track last request to avoid duplicates
  const lastRequestRef = useRef({ start: 0, end: 0 });
  
  // Heat configuration state (per-column min/max/enabled)
  const [heatConfig, setHeatConfig] = useState({});
  // Per-column heat style and color
  const [heatStyleByKey, setHeatStyleByKey] = useState({});
  const [heatColorByKey, setHeatColorByKey] = useState({});
  // Min wall (duration) filter - shown in duration_ms header settings
  const [minWallFilterMs, setMinWallFilterMs] = useState("");
  
  // Heat config handlers
  const toggleHeat = useCallback((key) => {
    setHeatConfig(prev => ({
      ...prev,
      [key]: { ...prev[key], enabled: !(prev[key]?.enabled ?? false) }
    }));
  }, []);
  
  const setHeatMin = useCallback((key, val) => {
    setHeatConfig(prev => ({
      ...prev,
      [key]: { ...prev[key], min: val === "" || val === null ? null : Number(val) }
    }));
  }, []);
  
  const setHeatMax = useCallback((key, val) => {
    setHeatConfig(prev => ({
      ...prev,
      [key]: { ...prev[key], max: val === "" || val === null ? null : Number(val) }
    }));
  }, []);
  
  const setHeatStyleForKey = useCallback((key, style) => {
    setHeatStyleByKey(prev => ({ ...prev, [key]: style }));
  }, []);
  
  const setHeatColorForKey = useCallback((key, color) => {
    setHeatColorByKey(prev => ({ ...prev, [key]: color }));
  }, []);

  const [heatMaxes, setHeatMaxes] = useState(DEFAULT_HEAT_MAXES);

  const effectiveMinDuration = useMemo(() => {
    return Math.max(
      filters.minDurationMs ? Number(filters.minDurationMs) : 0,
      minWallFilterMs ? Number(minWallFilterMs) : 0
    );
  }, [filters.minDurationMs, minWallFilterMs]);

  const requestAutocomplete = useCallback((type, query) => {
    if (!runId || !query) {
      setAutocomplete((prev) => ({ ...prev, [type]: [] }));
      setAutocompleteLoading((prev) => ({ ...prev, [type]: false }));
      return;
    }
    if (autocompleteAbortRef.current[type]) {
      autocompleteAbortRef.current[type].abort();
    }
    const controller = new AbortController();
    autocompleteAbortRef.current[type] = controller;
    setAutocompleteLoading((prev) => ({ ...prev, [type]: true }));

    getTransactionsAutocomplete(
      {
        run_id: runId,
        type: type === "contractFn" ? "function" : type,
        q: query,
        principal: filters.principal,
        contract: filters.contract,
        contract_fn: filters.contractFn,
      },
      { signal: controller.signal }
    )
      .then((data) => {
        setAutocomplete((prev) => ({ ...prev, [type]: data.values || [] }));
        setAutocompleteLoading((prev) => ({ ...prev, [type]: false }));
      })
      .catch((err) => {
        if (err.name !== "AbortError") {
          setAutocomplete((prev) => ({ ...prev, [type]: [] }));
          setAutocompleteLoading((prev) => ({ ...prev, [type]: false }));
        }
      });
  }, [runId, filters.principal, filters.contract, filters.contractFn]);

  useEffect(() => {
    const query = filterSearch.principal.trim();
    if (query.length < 2) {
      setAutocomplete((prev) => ({ ...prev, principal: [] }));
      setAutocompleteLoading((prev) => ({ ...prev, principal: false }));
      return;
    }
    const timer = setTimeout(() => requestAutocomplete("principal", query), 200);
    return () => clearTimeout(timer);
  }, [filterSearch.principal, requestAutocomplete]);

  useEffect(() => {
    const query = filterSearch.contract.trim();
    if (query.length < 2) {
      setAutocomplete((prev) => ({ ...prev, contract: [] }));
      setAutocompleteLoading((prev) => ({ ...prev, contract: false }));
      return;
    }
    const timer = setTimeout(() => requestAutocomplete("contract", query), 200);
    return () => clearTimeout(timer);
  }, [filterSearch.contract, requestAutocomplete]);

  useEffect(() => {
    const query = filterSearch.contractFn.trim();
    if (query.length < 2) {
      setAutocomplete((prev) => ({ ...prev, contractFn: [] }));
      setAutocompleteLoading((prev) => ({ ...prev, contractFn: false }));
      return;
    }
    const timer = setTimeout(() => requestAutocomplete("contractFn", query), 200);
    return () => clearTimeout(timer);
  }, [filterSearch.contractFn, requestAutocomplete]);

  // Build stable column definitions for svar-ui Grid.
  // These use context-aware components that fetch their configuration at render time,
  // allowing the column array to remain stable and preserve column positions/widths.
  const columns = useMemo(() => {
    // Helper to build a heat header with context-aware component
    const makeHeatHeader = (colKey, text, isWallColumn = false) => ({
      text,
      cell: TxContextHeatHeaderCell,
    });
    
    return [
      {
        id: "row_number",
        header: [
          { text: "", css: "grid-level1-empty" },
          { text: "", css: "grid-level2-empty" },
          { text: "#" },
        ],
        width: 60,
        resize: true,
        css: "grid-col-numeric grid-col-dimmed",
        template: (val) => (val == null ? "-" : String(val)),
      },
      {
        id: "_actions",
        header: [
          { text: "", css: "grid-level1-empty" },
          { text: "", css: "grid-level2-empty" },
          { text: "" },
        ],
        width: 50,
        cell: TxContextActionCell,
      },
      {
        id: "tx_hash_hex",
        header: [
          { text: "", css: "grid-level1-empty" },
          { text: "", css: "grid-level2-empty" },
          { text: "Transaction Hash" },
        ],
        width: 420,
        flexgrow: 1,
        sort: true,
        resize: true,
        cell: TxHashCell,
      },
      {
        id: "contract_issuer",
        header: [
          { text: "", css: "grid-level1-empty" },
          { text: "", css: "grid-level2-empty" },
          { text: "Issuer" },
        ],
        width: 140,
        resize: true,
        template: (val) => val ? truncateHash(val, 6) : "-",
        css: (row) => row.contract_issuer ? "" : "dimmed-cell",
      },
      {
        id: "contract_name",
        header: [
          { text: "", css: "grid-level1-empty" },
          { text: "", css: "grid-level2-empty" },
          { text: "Contract" },
        ],
        width: 160,
        resize: true,
        template: (val) => val || "-",
        css: (row) => row.contract_name ? "" : "dimmed-cell",
      },
      {
        id: "contract_fn",
        header: [
          { text: "", css: "grid-level1-empty" },
          { text: "", css: "grid-level2-empty" },
          { text: "Function" },
        ],
        width: 140,
        resize: true,
        template: (val) => val || "-",
        css: (row) => row.contract_fn ? "" : "dimmed-cell",
      },
      {
        id: "duration_ms",
        _colKey: "duration_ms",
        _decimals: 2,
        _isWallColumn: true,
        header: [
          { text: "", css: "grid-level1-empty" },
          { text: "", css: "grid-level2-empty" },
          makeHeatHeader("duration_ms", "Duration (ms)", true),
        ],
        width: 130,
        sort: true,
        resize: true,
        css: "grid-col-numeric",
        cell: TxContextHeatCell,
      },
      {
        id: "stacks_block_height",
        header: [
          { text: "", css: "grid-level1-empty" },
          { text: "", css: "grid-level2-empty" },
          { text: "Block" },
        ],
        width: 80,
        sort: true,
        resize: true,
        css: "grid-col-numeric",
        template: (val) => (val == null ? "-" : String(val)),
      },
      // ═══════════════════════════════════════════════════════════════════════════
      // Clarity - 5 columns: Runtime, Read (Count, Length), Write (Count, Length)
      // ═══════════════════════════════════════════════════════════════════════════
      {
        id: "clarity_runtime",
        _colKey: "clarity_runtime",
        _decimals: 0,
        header: [
          { text: "Clarity", colspan: 5, css: "grid-group-header grid-level1-header" },
          { text: "", css: "grid-level2-empty" },
          makeHeatHeader("clarity_runtime", "Runtime"),
        ],
        width: 90,
        sort: true,
        resize: true,
        css: "grid-col-numeric",
        cell: TxContextHeatCell,
      },
      {
        id: "clarity_read_count",
        _colKey: "clarity_read_count",
        _decimals: 0,
        header: [
          { text: "", _hidden: true },
          { text: "Read", colspan: 2, css: "grid-group-header grid-level2-header" },
          makeHeatHeader("clarity_read_count", "Count"),
        ],
        width: 80,
        sort: true,
        resize: true,
        css: "grid-col-numeric",
        cell: TxContextHeatCell,
      },
      {
        id: "clarity_read_length",
        _colKey: "clarity_read_length",
        _decimals: 0,
        header: [
          { text: "", _hidden: true },
          { text: "", _hidden: true },
          makeHeatHeader("clarity_read_length", "Length"),
        ],
        width: 90,
        sort: true,
        resize: true,
        css: "grid-col-numeric",
        cell: TxContextHeatCell,
      },
      {
        id: "clarity_write_count",
        _colKey: "clarity_write_count",
        _decimals: 0,
        header: [
          { text: "", _hidden: true },
          { text: "Write", colspan: 2, css: "grid-group-header grid-level2-header" },
          makeHeatHeader("clarity_write_count", "Count"),
        ],
        width: 80,
        sort: true,
        resize: true,
        css: "grid-col-numeric",
        cell: TxContextHeatCell,
      },
      {
        id: "clarity_write_length",
        _colKey: "clarity_write_length",
        _decimals: 0,
        header: [
          { text: "", _hidden: true },
          { text: "", _hidden: true },
          makeHeatHeader("clarity_write_length", "Length"),
        ],
        width: 90,
        sort: true,
        resize: true,
        css: "grid-col-numeric",
        cell: TxContextHeatCell,
      },
    ];
  }, []); // No dependencies - columns are now stable!

  const heatMaxAbortRef = useRef(null);
  const [heatMaxesLoaded, setHeatMaxesLoaded] = useState(false);

  useEffect(() => {
    if (!runId) {
      setHeatMaxes(DEFAULT_HEAT_MAXES);
      setHeatMaxesLoaded(true);
      return;
    }

    if (heatMaxAbortRef.current) {
      heatMaxAbortRef.current.abort();
    }
    const controller = new AbortController();
    heatMaxAbortRef.current = controller;

    const fetchHeatMaxes = async () => {
      try {
        setHeatMaxesLoaded(false);
        const data = await getTransactionsMaxes(
          {
            run_id: runId,
            principal: filters.principal,
            contract: filters.contract,
            contract_fn: filters.contractFn,
            min_duration_ms: effectiveMinDuration > 0 ? effectiveMinDuration : undefined,
          },
          { signal: controller.signal }
        );
        setHeatMaxes({ ...DEFAULT_HEAT_MAXES, ...(data.maxes || {}) });
      } catch (err) {
        if (err.name !== "AbortError") {
          setHeatMaxes(DEFAULT_HEAT_MAXES);
        }
      } finally {
        setHeatMaxesLoaded(true);
      }
    };

    fetchHeatMaxes();

    return () => {
      controller.abort();
    };
  }, [runId, filters.principal, filters.contract, filters.contractFn, effectiveMinDuration]);

  // Fetch a range of data from the server
  const fetchRange = useCallback(async (start, end) => {
    if (!runId) return;

    // Expand range to include buffer
    const fetchStart = Math.max(0, start - FETCH_BUFFER);
    const fetchEnd = end + FETCH_BUFFER;
    const fetchSize = Math.max(MIN_FETCH_SIZE, fetchEnd - fetchStart);

    // Create a range key to track what we've fetched
    const fetchEndInclusive = fetchStart + fetchSize;
    const alreadyFetched = fetchedRangesRef.current.some(
      (range) => fetchStart >= range.start && fetchEndInclusive <= range.end
    );
    if (alreadyFetched) {
      return; // Already fetched this range
    }

    // Cancel any existing request
    if (abortControllerRef.current) {
      abortControllerRef.current.abort();
    }

    const controller = new AbortController();
    abortControllerRef.current = controller;

    setIsLoading(true);

    try {
      const data = await getTransactions(
        {
          run_id: runId,
          offset: fetchStart,
          limit: fetchSize,
          sort_by: sortBy,
          sort_dir: sortDir,
          principal: filters.principal,
          contract: filters.contract,
          contract_fn: filters.contractFn,
          min_duration_ms: effectiveMinDuration > 0 ? effectiveMinDuration : undefined,
        },
        { signal: controller.signal }
      );
      
      // Update total count
      setTotal(data.total);
      
      // Mark this range as fetched
      fetchedRangesRef.current.push({ start: fetchStart, end: fetchEndInclusive });

      // Merge new data into cache (sparse array)
      setDataCache((prev) => {
        const next = [...prev];
        data.rows.forEach((row, i) => {
          const index = fetchStart + i;
          // Add unique id for the grid
          next[index] = {
            ...row,
            id: `${row.stacks_tx_id}-${row.synthetic_block_id}`,
            row_number: index + 1,
          };
        });
        return next;
      });

      setError(null);
    } catch (err) {
      if (err.name !== "AbortError") {
        setError(err.message);
      }
    } finally {
      setIsLoading(false);
      abortControllerRef.current = null;
    }
  }, [runId, sortBy, sortDir, filters, effectiveMinDuration]);

  // Reset cache when filters/sort/run changes
  useEffect(() => {
    setDataCache([]);
    setTotal(0);
    setVisibleRange({ start: 0, end: MIN_FETCH_SIZE });
    fetchedRangesRef.current = [];
    lastRequestRef.current = { start: 0, end: 0 };
    
    if (runId) {
      // Fetch initial data
      fetchRange(0, MIN_FETCH_SIZE);
    }
  }, [runId, sortBy, sortDir, filters, effectiveMinDuration, fetchRange]);

  // Handle dynamic data request from grid during scroll
  const handleRequestData = useCallback((ev) => {
    const { row } = ev;
    if (row) {
      const { start, end } = row;
      
      // Update visible range for rendering the correct slice
      setVisibleRange({ start, end });
      
      // Avoid duplicate requests for same range
      if (start === lastRequestRef.current.start && end === lastRequestRef.current.end) {
        return;
      }
      lastRequestRef.current = { start, end };

      const rangeCovered = fetchedRangesRef.current.some(
        (range) => start >= range.start && end <= range.end
      );
      if (!rangeCovered) {
        fetchRange(start, end);
      }
    }
  }, [fetchRange]);

  const [pendingMinDuration, setPendingMinDuration] = useState("");

  useEffect(() => {
    const timer = setTimeout(() => {
      setFilters((prev) => ({ ...prev, minDurationMs: pendingMinDuration }));
    }, 300);
    return () => clearTimeout(timer);
  }, [pendingMinDuration]);

  const updateFilterValues = useCallback((key, values) => {
    setFilters((prev) => ({ ...prev, [key]: values }));
  }, []);

  const removeFilterValue = useCallback((key, value) => {
    setFilters((prev) => ({ ...prev, [key]: prev[key].filter((item) => item !== value) }));
  }, []);

  const clearFilters = useCallback(() => {
    setFilters({ principal: [], contract: [], contractFn: [], minDurationMs: "" });
    setFilterSearch({ principal: "", contract: "", contractFn: "" });
    setPendingMinDuration("");
  }, []);

  const hasActiveFilters = useMemo(() => {
    return (
      filters.principal.length ||
      filters.contract.length ||
      filters.contractFn.length ||
      filters.minDurationMs
    );
  }, [filters]);

  // Grid init callback
  const handleInit = useCallback((api) => {
    gridApiRef.current = api;
  }, []);

  const hasAnyData = useMemo(() => dataCache.some(Boolean), [dataCache]);

  // Compute visible data slice for the grid (svar-ui expects only the visible rows)
  const visibleData = useMemo(() => {
    const { start, end } = visibleRange;
    return dataCache.slice(start, end);
  }, [dataCache, visibleRange]);

  // Build callbacks object for the context provider.
  // These callbacks are accessed via context to avoid recreating column definitions.
  const handleViewTrace = useCallback((row) => {
    onViewTrace(row.tx_hash_hex, row.stacks_tx_id);
  }, [onViewTrace]);

  const gridCallbacks = {
    onViewTrace: handleViewTrace,
    heatConfig,
    heatMaxes,
    heatStyleByKey,
    heatColorByKey,
    numberFormat,
    toggleHeat,
    setHeatMin,
    setHeatMax,
    setHeatStyleForKey,
    setHeatColorForKey,
    heatColorOptions: HEAT_COLOR_OPTIONS,
    minWallFilterMs,
    setMinWallFilterMs,
  };

  return (
    <div className="transactions-tab">
      {/* Toolbar */}
      <div className="transactions-toolbar">
        <div className="transactions-toolbar-left">
          <Button
            variant={showFilters ? "default" : "outline"}
            size="sm"
            className="gap-2"
            onClick={() => setShowFilters(!showFilters)}
          >
            <Filter className="h-4 w-4" />
            Filters
            {hasActiveFilters && (
              <span className="transactions-filter-badge">•</span>
            )}
          </Button>
          {hasActiveFilters && (
            <Button
              variant="ghost"
              size="sm"
              className="gap-1 text-muted-foreground"
              onClick={clearFilters}
            >
              <X className="h-3 w-3" />
              Clear
            </Button>
          )}
        </div>
        <div className="transactions-toolbar-right">
          <span className="transactions-count">
            {isLoading ? (
              <Loader2 className="h-4 w-4 animate-spin" />
            ) : (
              `${total.toLocaleString()} transactions`
            )}
          </span>
        </div>
      </div>

      {/* Filters Panel */}
      {showFilters && (
        <div className="transactions-filters">
          <div className="transactions-filter-field">
            <label>Issuer/Principal</label>
            <Combobox
              options={autocomplete.principal.map((value) => ({ label: value, value }))}
              value={filters.principal}
              onChange={(values) => updateFilterValues("principal", values)}
              multiple
              showClear
              placeholder="Select principals"
              searchPlaceholder="Filter by address..."
              loading={autocompleteLoading.principal}
              onSearch={(value) =>
                setFilterSearch((prev) => ({ ...prev, principal: value }))
              }
            />
          </div>
          <div className="transactions-filter-field">
            <label>Contract</label>
            <Combobox
              options={autocomplete.contract.map((value) => ({ label: value, value }))}
              value={filters.contract}
              onChange={(values) => updateFilterValues("contract", values)}
              multiple
              showClear
              placeholder="Select contracts"
              searchPlaceholder="Filter by contract name..."
              loading={autocompleteLoading.contract}
              onSearch={(value) =>
                setFilterSearch((prev) => ({ ...prev, contract: value }))
              }
            />
          </div>
          <div className="transactions-filter-field">
            <label>Function</label>
            <Combobox
              options={autocomplete.contractFn.map((value) => ({ label: value, value }))}
              value={filters.contractFn}
              onChange={(values) => updateFilterValues("contractFn", values)}
              multiple
              showClear
              placeholder="Select functions"
              searchPlaceholder="Filter by function name..."
              loading={autocompleteLoading.contractFn}
              onSearch={(value) =>
                setFilterSearch((prev) => ({ ...prev, contractFn: value }))
              }
            />
          </div>
          <div className="transactions-filter-field">
            <label>Min Duration (ms)</label>
            <input
              type="number"
              placeholder="0"
              value={pendingMinDuration}
              onChange={(e) => setPendingMinDuration(e.target.value)}
              className="transactions-filter-number"
            />
          </div>
        </div>
      )}

      {/* Grid Container */}
      <div className="transactions-grid-container">
        {error ? (
          <div className="transactions-error">
            <span>Error: {error}</span>
            <Button variant="outline" size="sm" onClick={() => fetchRange(0, MIN_FETCH_SIZE)}>
              Retry
            </Button>
          </div>
        ) : (
          <WillowDark>
            <TransactionsGridProvider callbacks={gridCallbacks}>
              <Grid
                data={visibleData}
                columns={columns}
                sizes={{ rowHeight: 36 }}
                dynamic={total > 0 ? { rowCount: total } : null}
                onRequestData={handleRequestData}
                init={handleInit}
                columnStyle={(column) => column.css || ""}
                overlay={isLoading && !hasAnyData || !heatMaxesLoaded ? "Loading transactions..." : null}
              />
            </TransactionsGridProvider>
          </WillowDark>
        )}
      </div>
    </div>
  );
}
