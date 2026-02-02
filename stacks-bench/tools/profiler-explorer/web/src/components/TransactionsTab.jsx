import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Grid } from "@svar-ui/react-grid";
import { WillowDark } from "@svar-ui/react-core";
import {
  Loader2,
  ExternalLink,
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
import { HeatCell } from "./HeatCells";
import HeatHeaderCell, { HeatHeaderMenuContext } from "./HeatHeaderCell";
import { DEFAULT_NUMBER_FORMAT_ID, NUMBER_FORMATS, DEFAULT_HEAT_COLOR, DEFAULT_HEAT_STYLE, HEAT_COLOR_OPTIONS } from "../profilerConfig.ts";

// Buffer size - how many rows to fetch beyond visible area
const FETCH_BUFFER = 100;
// Minimum rows to fetch per request
const MIN_FETCH_SIZE = 200;

// Use the same number format as profiler trace grid
function getNumberFormat(id) {
  return NUMBER_FORMATS.find((format) => format.id === id) || NUMBER_FORMATS[2];
}

function truncateHash(hash, len = 12) {
  if (!hash) return "-";
  if (hash.length <= len * 2) return hash;
  return `${hash.slice(0, len)}…${hash.slice(-len)}`;
}

function formatNumber(value, format, decimals = 0) {
  if (value === null || value === undefined) return "-";
  if (typeof value !== "number" || !Number.isFinite(value)) return String(value);
  if (value === 0) return "-";
  const sign = value < 0 ? "-" : "";
  const abs = Math.abs(value);
  const fixed = Number.isFinite(decimals) ? abs.toFixed(decimals) : String(abs);
  const [intPart, fracPart] = fixed.split(".");
  const grouped = format.group
    ? intPart.replace(/\B(?=(\d{3})+(?!\d))/g, format.group)
    : intPart;
  return `${sign}${grouped}${fracPart ? `${format.decimal}${fracPart}` : ""}`;
}

// Cell component for transaction hash with click handler
function TxHashCell({ row, onViewTrace }) {
  return (
    <button
      className="tx-hash-link"
      onClick={(e) => {
        e.stopPropagation();
        onViewTrace(row);
      }}
      title={row.tx_hash_hex}
    >
      {row.tx_hash_hex || "-"}
    </button>
  );
}

// Action cell with view trace button
function ActionCell({ row, onViewTrace }) {
  return (
    <button
      className="transactions-view-btn"
      onClick={(e) => {
        e.stopPropagation();
        onViewTrace(row);
      }}
      title="View trace"
    >
      <ExternalLink className="h-4 w-4" />
    </button>
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

export default function TransactionsTab({ runId, onViewTrace }) {
  // Grid data - sparse array indexed by row position
  const [dataCache, setDataCache] = useState([]);
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
  const [heatMenuOpenId, setHeatMenuOpenId] = useState(null);
  // Per-column heat style and color
  const [heatStyleByKey, setHeatStyleByKey] = useState({});
  const [heatColorByKey, setHeatColorByKey] = useState({});
  // Min wall (duration) filter - shown in duration_ms header settings
  const [minWallFilterMs, setMinWallFilterMs] = useState("");
  
  // Heat config handlers
  const toggleHeat = useCallback((key) => {
    setHeatConfig(prev => ({
      ...prev,
      [key]: { ...prev[key], enabled: !(prev[key]?.enabled ?? true) }
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
  
  // Number format (same as profiler trace)
  const numberFormat = useMemo(() => getNumberFormat(DEFAULT_NUMBER_FORMAT_ID), []);

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

  // Build columns for svar-ui Grid
  const columns = useMemo(() => {
    const handleView = (row) => onViewTrace(row.tx_hash_hex, row.stacks_tx_id);
    
    // Helper to create heat cell for a numeric column
    const makeHeatCell = (key, decimals = 0) => (props) => {
      const { row } = props;
      const value = row[key];
      const config = heatConfig[key] || { enabled: true, min: null, max: null };
      const colHeatStyle = heatStyleByKey[key] || DEFAULT_HEAT_STYLE;
      const colHeatColor = heatColorByKey[key] || DEFAULT_HEAT_COLOR;
      if (!config.enabled) {
        // No heat, just show the value
        return (
          <span className="numeric-cell">
            {formatNumber(value, numberFormat, decimals)}
          </span>
        );
      }
      const minVal = config.min ?? 0;
      const maxVal = config.max ?? heatMaxes[key] ?? 1;
      const range = maxVal - minVal || 1;
      const percent = typeof value === "number" ? Math.max(0, Math.min(100, ((value - minVal) / range) * 100)) : 0;
      return (
        <HeatCell
          row={row}
          value={value}
          percent={percent}
          format={{ decimals }}
          numberFormat={numberFormat}
          heatStyle={colHeatStyle}
          heatColor={colHeatColor}
        />
      );
    };
    
    // Helper to create heat header cell with settings popover
    const makeHeatHeader = (key, text, isWallColumn = false) => ({
      text,
      cell: (props) => (
        <HeatHeaderCell
          {...props}
          column={{ id: key, heatKey: key }}
          cell={{ text }}
          heatConfig={heatConfig}
          onToggle={toggleHeat}
          onMinChange={setHeatMin}
          onMaxChange={setHeatMax}
          heatStyle={heatStyleByKey[key] || DEFAULT_HEAT_STYLE}
          setHeatStyle={setHeatStyleForKey}
          heatColor={heatColorByKey[key] || DEFAULT_HEAT_COLOR}
          setHeatColor={setHeatColorForKey}
          heatColorOptions={HEAT_COLOR_OPTIONS}
          minWallFilterMs={isWallColumn ? minWallFilterMs : undefined}
          setMinWallFilterMs={isWallColumn ? setMinWallFilterMs : undefined}
        />
      ),
    });
    
    return [
      {
        id: "tx_hash_hex",
        header: "Transaction Hash",
        width: 420,
        flexgrow: 1,
        sort: true,
        resize: true,
        cell: (props) => <TxHashCell {...props} onViewTrace={handleView} />,
      },
      {
        id: "contract_issuer",
        header: "Issuer",
        width: 140,
        resize: true,
        template: (val) => val ? truncateHash(val, 6) : "-",
        css: (row) => row.contract_issuer ? "" : "dimmed-cell",
      },
      {
        id: "contract_name",
        header: "Contract",
        width: 160,
        resize: true,
        template: (val) => val || "-",
        css: (row) => row.contract_name ? "" : "dimmed-cell",
      },
      {
        id: "contract_fn",
        header: "Function",
        width: 140,
        resize: true,
        template: (val) => val || "-",
        css: (row) => row.contract_fn ? "" : "dimmed-cell",
      },
      {
        id: "duration_ms",
        heatKey: "duration_ms",
        header: makeHeatHeader("duration_ms", "Duration (ms)", true),
        width: 130,
        sort: true,
        resize: true,
        cell: makeHeatCell("duration_ms", 2),
      },
      {
        id: "stacks_block_height",
        header: "Block",
        width: 80,
        sort: true,
        resize: true,
        css: "numeric",
        template: (val) => (val == null ? "-" : String(val)),
      },
      // Clarity metrics group
      {
        id: "clarity_runtime",
        heatKey: "clarity_runtime",
        header: [
          { text: "Clarity", colspan: 5, css: "grid-group-header" },
          makeHeatHeader("clarity_runtime", "Runtime"),
        ],
        width: 100,
        sort: true,
        resize: true,
        cell: makeHeatCell("clarity_runtime", 0),
      },
      {
        id: "clarity_read_count",
        heatKey: "clarity_read_count",
        header: ["", makeHeatHeader("clarity_read_count", "Reads")],
        width: 80,
        sort: true,
        resize: true,
        cell: makeHeatCell("clarity_read_count", 0),
      },
      {
        id: "clarity_read_length",
        heatKey: "clarity_read_length",
        header: ["", makeHeatHeader("clarity_read_length", "Read Len")],
        width: 100,
        sort: true,
        resize: true,
        cell: makeHeatCell("clarity_read_length", 0),
      },
      {
        id: "clarity_write_count",
        heatKey: "clarity_write_count",
        header: ["", makeHeatHeader("clarity_write_count", "Writes")],
        width: 80,
        sort: true,
        resize: true,
        cell: makeHeatCell("clarity_write_count", 0),
      },
      {
        id: "clarity_write_length",
        heatKey: "clarity_write_length",
        header: ["", makeHeatHeader("clarity_write_length", "Write Len")],
        width: 100,
        sort: true,
        resize: true,
        cell: makeHeatCell("clarity_write_length", 0),
      },
      {
        id: "_actions",
        header: "",
        width: 50,
        cell: (props) => <ActionCell {...props} onViewTrace={handleView} />,
      },
    ];
  }, [onViewTrace, heatMaxes, numberFormat, heatConfig, heatStyleByKey, heatColorByKey, toggleHeat, setHeatMin, setHeatMax, setHeatStyleForKey, setHeatColorForKey, minWallFilterMs]);

  const heatMaxAbortRef = useRef(null);

  useEffect(() => {
    if (!runId) {
      setHeatMaxes(DEFAULT_HEAT_MAXES);
      return;
    }

    if (heatMaxAbortRef.current) {
      heatMaxAbortRef.current.abort();
    }
    const controller = new AbortController();
    heatMaxAbortRef.current = controller;

    const fetchHeatMaxes = async () => {
      try {
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
          next[index] = { ...row, id: `${row.stacks_tx_id}-${row.synthetic_block_id}` };
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

  const rowById = useMemo(() => {
    const map = new Map();
    dataCache.forEach((row) => {
      if (row && row.id) {
        map.set(row.id, row);
      }
    });
    return map;
  }, [dataCache]);

  // Handle row selection - navigate to trace
  const handleSelectRow = useCallback((ev) => {
    const { id } = ev;
    if (id) {
      const row = rowById.get(id);
      if (row) {
        onViewTrace(row.tx_hash_hex, row.stacks_tx_id);
      }
    }
  }, [rowById, onViewTrace]);

  // Grid init callback
  const handleInit = useCallback((api) => {
    gridApiRef.current = api;
  }, []);

  const hasAnyData = useMemo(() => dataCache.some(Boolean), [dataCache]);

  const heatMenuValue = useMemo(
    () => ({ openId: heatMenuOpenId, setOpenId: setHeatMenuOpenId }),
    [heatMenuOpenId, setHeatMenuOpenId]
  );

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
            <HeatHeaderMenuContext.Provider value={heatMenuValue}>
              <Grid
                data={dataCache}
                columns={columns}
                sizes={{ rowHeight: 36 }}
                dynamic={total > 0 ? { rowCount: total } : null}
                onRequestData={handleRequestData}
                onSelectRow={handleSelectRow}
                init={handleInit}
                select={true}
                overlay={isLoading && !hasAnyData ? "Loading transactions..." : null}
              />
            </HeatHeaderMenuContext.Provider>
          </WillowDark>
        )}
      </div>
    </div>
  );
}
