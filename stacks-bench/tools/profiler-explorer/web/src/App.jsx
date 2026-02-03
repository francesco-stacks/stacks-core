import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import ProfilerGrid from "./components/ProfilerGrid";
import SettingsPanel from "./components/SettingsPanel";
import TransactionsTab from "./components/TransactionsTab";
import SpanDetailsModal from "./components/SpanDetailsModal";
import SpanCell from "./components/SpanCell";
import { HeatCell, NumericCell } from "./components/HeatCells";
import HeaderBar from "./components/HeaderBar";
import ToolbarBar from "./components/ToolbarBar";
import BreadcrumbBar from "./components/BreadcrumbBar";
import {
  createHeatHeaderBuilder,
  createSpanHeaderBuilder,
} from "./columnBuilders";
import {
  applyChainCompression,
  applyFocus,
  applyHotPath,
  applyOpenState,
  buildTreeIndex,
  collectSubtreeIds,
  computeDefaultOpenSet,
  flattenTree,
  getSelfCpuUs,
  getSelfWaitUs,
  getSelfWallUs,
  getWallUs,
  indexTree,
  pruneTree,
} from "./treeTransforms.ts";
import {
  ALWAYS_HIDDEN_KEYS,
  ALWAYS_VISIBLE_KEYS,
  COLUMN_DEFS,
  COUNT_DIM_KEYS,
  DEFAULT_COLUMNS,
  NUMERIC_COLUMN_KEYS,
  sanitizeSelectedColumns,
} from "./columnsConfig.ts";
import {
  DEFAULT_AUTO_EXPAND,
  DEFAULT_HEAT_COLOR,
  DEFAULT_HEAT_STYLE,
  DEFAULT_NUMBER_FORMAT_ID,
  HEAT_COLOR_OPTIONS,
  NUMBER_FORMATS,
  THEME_PRESETS,
} from "./profilerConfig.ts";
import { getBlocks, getRuns, getTrace, lookupTx } from "./lib/api.ts";



function getNumberFormat(id) {
  return NUMBER_FORMATS.find((format) => format.id === id) || NUMBER_FORMATS[2];
}

function getHeatKeyForColumn(col) {
  if (col.heatKey) return col.heatKey;
  return NUMERIC_COLUMN_KEYS.has(col.key) ? col.key : null;
}

function buildDefaultHeatConfig() {
  const defaults = {};
  for (const col of COLUMN_DEFS) {
    const heatKey = getHeatKeyForColumn(col);
    if (!heatKey || defaults[heatKey]) continue;
    defaults[heatKey] = { enabled: false, min: null, max: null };
  }
  return defaults;
}

function buildDefaultHeatStyleMap() {
  const legacy = localStorage.getItem("profilerHeatStyle") || DEFAULT_HEAT_STYLE;
  const defaults = {};
  for (const col of COLUMN_DEFS) {
    const heatKey = getHeatKeyForColumn(col);
    if (!heatKey || defaults[heatKey]) continue;
    defaults[heatKey] = legacy;
  }
  return defaults;
}

function buildDefaultHeatColorMap() {
  const defaults = {};
  for (const col of COLUMN_DEFS) {
    const heatKey = getHeatKeyForColumn(col);
    if (!heatKey || defaults[heatKey]) continue;
    defaults[heatKey] = DEFAULT_HEAT_COLOR;
  }
  return defaults;
}

const DEFAULT_NUMERIC_WIDTH = 100;
const MIN_NUMERIC_WIDTH = 90;

function getSafeStorage() {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

export default function App() {
  const [runs, setRuns] = useState([]);
  const [blocks, setBlocks] = useState([]);
  const [runId, setRunId] = useState("");
  const [activeTab, setActiveTab] = useState("transactions"); // "trace" | "transactions"
  const [mode, setMode] = useState("tx");
  const [txQuery, setTxQuery] = useState("");
  const [stacksTxId, setStacksTxId] = useState("");
  const [stacksBlockId, setStacksBlockId] = useState("");
  const [minWallMs, setMinWallMs] = useState("");
  const [segmentRootId, setSegmentRootId] = useState("");
  const [limit, setLimit] = useState("5000");
  const [rows, setRows] = useState([]);
  const [summary, setSummary] = useState("");
  const [chainCompression, setChainCompression] = useState(true);
  const [hotPathMode, setHotPathMode] = useState("off");
  const [focusId, setFocusId] = useState(null);
  const [activeId, setActiveId] = useState(null);
  const [spanDetailsOpen, setSpanDetailsOpen] = useState(false);
  const [openNodes, setOpenNodes] = useState(new Set());
  const [expandedChains, setExpandedChains] = useState(new Set());
  const [heatMenuOpenId, setHeatMenuOpenId] = useState(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [theme, setTheme] = useState(() => {
    return localStorage.getItem("profilerTheme") || "dark";
  });
  const [minWallFilterMs, setMinWallFilterMs] = useState(() => {
    return localStorage.getItem("profilerWallMinFilterMs") || "";
  });
  const [themePreset, setThemePreset] = useState(() => {
    return localStorage.getItem("profilerThemePreset") || "default";
  });
  const [isLoading, setIsLoading] = useState(false);
  const abortControllerRef = useRef(null);
  const [lastLoadedQuery, setLastLoadedQuery] = useState(null);
  const setTab = useCallback((tab, { replace = false } = {}) => {
    setActiveTab(tab);
    const url = new URL(window.location.href);
    url.hash = tab === "trace" ? "trace" : "transactions";
    if (replace) {
      window.history.replaceState({ tab }, "", url);
    } else {
      window.history.pushState({ tab }, "", url);
    }
  }, []);
  const [heatConfig, setHeatConfig] = useState(() => {
    const defaults = buildDefaultHeatConfig();
    const storage = getSafeStorage();
    const stored = storage?.getItem("profilerHeatConfig");
    if (stored) {
      try {
        return { ...defaults, ...JSON.parse(stored) };
      } catch {
        return defaults;
      }
    }
    return defaults;
  });
  const [heatStyleByKey, setHeatStyleByKey] = useState(() => {
    const defaults = buildDefaultHeatStyleMap();
    const storage = getSafeStorage();
    const stored = storage?.getItem("profilerHeatStyleByKey");
    if (stored) {
      try {
        return { ...defaults, ...JSON.parse(stored) };
      } catch {
        return defaults;
      }
    }
    return defaults;
  });
  const [heatColorByKey, setHeatColorByKey] = useState(() => {
    const defaults = buildDefaultHeatColorMap();
    const storage = getSafeStorage();
    const stored = storage?.getItem("profilerHeatColorByKey");
    if (stored) {
      try {
        return { ...defaults, ...JSON.parse(stored) };
      } catch {
        return defaults;
      }
    }
    return defaults;
  });
  const [selectedColumns, setSelectedColumns] = useState(() => {
    const storage = getSafeStorage();
    const stored = storage?.getItem("profilerColumns");
    if (!stored) return sanitizeSelectedColumns(DEFAULT_COLUMNS);
    try {
      const parsed = JSON.parse(stored);
      return sanitizeSelectedColumns(parsed);
    } catch {
      return sanitizeSelectedColumns(DEFAULT_COLUMNS);
    }
  });
  const [numberFormatId, setNumberFormatId] = useState(() => {
    const storage = getSafeStorage();
    return storage?.getItem("profilerNumberFormat") || DEFAULT_NUMBER_FORMAT_ID;
  });
  const [spanVizConfig, setSpanVizConfig] = useState(() => {
    const storage = getSafeStorage();
    const stored = storage?.getItem("profilerSpanVizConfig");
    if (stored) {
      try {
        return JSON.parse(stored);
      } catch {
        return { enabled: true, style: "fill", metric: "wallTotalUs" };
      }
    }
    return { enabled: true, style: "fill", metric: "wallTotalUs" };
  });
  const [spanVizMenuOpen, setSpanVizMenuOpen] = useState(false);
  const numberFormat = useMemo(() => getNumberFormat(numberFormatId), [numberFormatId]);

  const gridApiRef = useRef(null);

  useEffect(() => {
    const storage = getSafeStorage();
    storage?.setItem("profilerHeatConfig", JSON.stringify(heatConfig));
  }, [heatConfig]);

  useEffect(() => {
    const storage = getSafeStorage();
    storage?.setItem("profilerTheme", theme);
  }, [theme]);

  useEffect(() => {
    const storage = getSafeStorage();
    storage?.setItem("profilerWallMinFilterMs", minWallFilterMs);
  }, [minWallFilterMs]);

  useEffect(() => {
    const storage = getSafeStorage();
    storage?.setItem("profilerThemePreset", themePreset);
  }, [themePreset]);

  useEffect(() => {
    const root = document.documentElement;
    if (theme === "dark") {
      root.classList.add("dark");
    } else {
      root.classList.remove("dark");
    }
  }, [theme]);

  useEffect(() => {
    const root = document.documentElement;
    if (themePreset === "default") {
      root.removeAttribute("data-theme");
    } else {
      root.setAttribute("data-theme", themePreset);
    }
  }, [themePreset]);

  const toggleTheme = useCallback(() => {
    setTheme((prev) => (prev === "dark" ? "light" : "dark"));
  }, []);

  useEffect(() => {
    const storage = getSafeStorage();
    storage?.setItem("profilerNumberFormat", numberFormatId);
  }, [numberFormatId]);

  useEffect(() => {
    const storage = getSafeStorage();
    storage?.setItem("profilerHeatStyleByKey", JSON.stringify(heatStyleByKey));
  }, [heatStyleByKey]);

  useEffect(() => {
    const storage = getSafeStorage();
    storage?.setItem("profilerHeatColorByKey", JSON.stringify(heatColorByKey));
  }, [heatColorByKey]);

  useEffect(() => {
    const storage = getSafeStorage();
    storage?.setItem("profilerSpanVizConfig", JSON.stringify(spanVizConfig));
  }, [spanVizConfig]);

  useEffect(() => {
    const handleClick = () => {
      setHeatMenuOpenId(null);
      setSpanVizMenuOpen(false);
    };
    document.addEventListener("click", handleClick);
    return () => document.removeEventListener("click", handleClick);
  }, []);

  useEffect(() => {
    getRuns()
      .then((data) => {
        setRuns(data);
        if (data.length > 0) setRunId(String(data[0].id));
      })
      .catch((err) => setSummary(err.message));
  }, []);

  useEffect(() => {
    if (!runId) return;
    getBlocks(runId)
      .then(setBlocks)
      .catch(() => setBlocks([]));
  }, [runId]);

  useEffect(() => {
    const initialTab = window.location.hash === "#trace" ? "trace" : "transactions";
    setActiveTab(initialTab);
    window.history.replaceState({ tab: initialTab }, "", window.location.href);

    const handlePopState = (event) => {
      const tab = event.state?.tab || (window.location.hash === "#trace" ? "trace" : "transactions");
      setActiveTab(tab);
    };
    window.addEventListener("popstate", handlePopState);
    return () => window.removeEventListener("popstate", handlePopState);
  }, []);

  useEffect(() => {
    if (!runId) return;
    const normalized = txQuery.trim().toLowerCase();
    if (normalized.length !== 64) return;
    lookupTx(runId, normalized)
      .then((data) => {
        if (data?.stacks_tx_id) {
          setStacksTxId(String(data.stacks_tx_id));
        } else {
          setSummary("Tx hash not found in this run.");
        }
      })
      .catch((err) => setSummary(err.message));
  }, [runId, txQuery]);

  const loadTrace = useCallback(async () => {
    if (!runId) return;
    
    // Switch to trace tab when loading trace
    setTab("trace");
    
    // Cancel any existing request
    if (abortControllerRef.current) {
      abortControllerRef.current.abort();
    }
    
    const controller = new AbortController();
    abortControllerRef.current = controller;
    
    const params = {
      run_id: runId,
      mode,
      limit: limit || "5000",
      stacks_tx_id: mode === "tx" && stacksTxId ? stacksTxId : undefined,
      stacks_block_id: mode === "run" && stacksBlockId ? stacksBlockId : undefined,
      segment_root_id: mode === "run" && segmentRootId ? segmentRootId : undefined,
      min_wall_ms: minWallMs || undefined,
    };

    // Clear data immediately when starting a new search
    setRows([]);
    setSummary("Loading trace...");
    setIsLoading(true);
    try {
      const data = await getTrace(params, { signal: controller.signal });
      setRows(data);
      if (data.length === 0) {
        setSummary("No data found.");
      } else {
        setSummary(`${data.length} records loaded.`);
      }
      setLastLoadedQuery({
        runId,
        mode,
        txQuery,
        stacksTxId,
        stacksBlockId,
        segmentRootId,
        minWallMs,
        limit,
        hotPathMode,
      });
    } catch (err) {
      if (err.name === "AbortError") {
        setSummary("Request cancelled.");
      } else {
        setSummary(`Error: ${err.message}`);
      }
    } finally {
      setIsLoading(false);
      abortControllerRef.current = null;
    }
  }, [runId, mode, limit, stacksTxId, stacksBlockId, segmentRootId, minWallMs, txQuery, hotPathMode, setTab]);

  const cancelLoad = useCallback(() => {
    if (abortControllerRef.current) {
      abortControllerRef.current.abort();
    }
  }, []);

  const isDirty = useMemo(() => {
    if (!lastLoadedQuery) return true;
    return (
      lastLoadedQuery.runId !== runId ||
      lastLoadedQuery.mode !== mode ||
      lastLoadedQuery.txQuery !== txQuery ||
      lastLoadedQuery.stacksTxId !== stacksTxId ||
      lastLoadedQuery.stacksBlockId !== stacksBlockId ||
      lastLoadedQuery.segmentRootId !== segmentRootId ||
      lastLoadedQuery.minWallMs !== minWallMs ||
      lastLoadedQuery.limit !== limit ||
      lastLoadedQuery.hotPathMode !== hotPathMode
    );
  }, [lastLoadedQuery, runId, mode, txQuery, stacksTxId, stacksBlockId, segmentRootId, minWallMs, limit, hotPathMode]);

  const resetQuery = useCallback(() => {
    setMode("tx");
    setTxQuery("");
    setStacksTxId("");
    setStacksBlockId("");
    setSegmentRootId("");
    setMinWallMs("");
    setMinWallFilterMs("");
    setLimit("5000");
    setHotPathMode("off");
    setChainCompression(true);
  }, []);

  // Navigate to trace view for a specific transaction
  const viewTransactionTrace = useCallback((txHashHex, stacksTxIdValue) => {
    setTxQuery(txHashHex);
    setStacksTxId(String(stacksTxIdValue));
    setTab("trace");
    // Trigger load after switching to trace tab
    setTimeout(() => {
      const params = {
        run_id: runId,
        mode: "tx",
        stacks_tx_id: String(stacksTxIdValue),
        limit: limit || "5000",
      };

      setRows([]);
      setSummary("Loading trace...");
      setIsLoading(true);

      getTrace(params)
        .then((data) => {
          setRows(data);
          setSummary(data.length === 0 ? "No data found." : `${data.length} records loaded.`);
          setLastLoadedQuery({
            runId,
            mode: "tx",
            txQuery: txHashHex,
            stacksTxId: String(stacksTxIdValue),
            stacksBlockId: "",
            segmentRootId: "",
            minWallMs: "",
            limit,
            hotPathMode,
          });
        })
        .catch((err) => {
          setSummary(`Error: ${err.message}`);
        })
        .finally(() => {
          setIsLoading(false);
        });
    }, 50);
  }, [runId, limit, hotPathMode, setTab]);

  const maxWallUs = useMemo(() => {
    return rows.reduce((maxVal, row) => {
      const wall = getWallUs(row);
      if (wall == null) return maxVal;
      return Math.max(maxVal, wall);
    }, 0);
  }, [rows]);

  const baseRows = useMemo(() => {
    return rows.map((row) => ({
      ...row,
      flame_percent: maxWallUs > 0 ? ((getWallUs(row) ?? 0) / maxWallUs) * 100 : 0,
    }));
  }, [rows, maxWallUs]);

  const baseTree = useMemo(() => buildTreeIndex(baseRows), [baseRows]);

  const minWallFilterUs = useMemo(
    () => (minWallFilterMs ? Number(minWallFilterMs) * 1000 : null),
    [minWallFilterMs]
  );

  const prunedRoots = useMemo(
    () => pruneTree(baseTree.roots, minWallFilterUs),
    [baseTree.roots, minWallFilterUs]
  );

  const prunedById = useMemo(() => indexTree(prunedRoots), [prunedRoots]);

  const focusedTree = useMemo(
    () => applyFocus(prunedRoots, prunedById, focusId),
    [prunedRoots, prunedById, focusId]
  );

  const hotPathRoots = useMemo(
    () => applyHotPath(focusedTree.roots, hotPathMode),
    [focusedTree.roots, hotPathMode]
  );

  const compressedRoots = useMemo(
    () =>
      applyChainCompression(hotPathRoots, {
        enabled: chainCompression,
        expandedChains,
        significantSelfUs: DEFAULT_AUTO_EXPAND.selfMs * 1000,
      }),
    [hotPathRoots, chainCompression, expandedChains]
  );

  const withFlamePercent = useMemo(() => {
    const applyPercent = (node) => {
      const wall = getWallUs(node) ?? 0;
      const flame_percent = maxWallUs > 0 ? (wall / maxWallUs) * 100 : 0;
      return {
        ...node,
        flame_percent,
        data: (node.data || []).map(applyPercent),
      };
    };
    return compressedRoots.map(applyPercent);
  }, [compressedRoots, maxWallUs]);

  const roots = useMemo(
    () => applyOpenState(withFlamePercent, openNodes, hotPathMode !== "off"),
    [withFlamePercent, openNodes, hotPathMode]
  );

  const toggleHeat = useCallback((heatKey) => {
    setHeatConfig((prev) => ({
      ...prev,
      [heatKey]: {
        ...(prev[heatKey] || {}),
        enabled: !(prev[heatKey]?.enabled ?? true),
      },
    }));
  }, []);

  const setHeatMin = useCallback((heatKey, value) => {
    const next = value === "" ? null : Number(value);
    setHeatConfig((prev) => ({
      ...prev,
      [heatKey]: {
        ...(prev[heatKey] || {}),
        min: Number.isNaN(next) ? null : next,
      },
    }));
  }, []);

  const setHeatMax = useCallback((heatKey, value) => {
    const next = value === "" ? null : Number(value);
    setHeatConfig((prev) => ({
      ...prev,
      [heatKey]: {
        ...(prev[heatKey] || {}),
        max: Number.isNaN(next) ? null : next,
      },
    }));
  }, []);

  const setHeatStyleForKey = useCallback((heatKey, value) => {
    setHeatStyleByKey((prev) => ({
      ...prev,
      [heatKey]: value,
    }));
  }, []);

  const setHeatColorForKey = useCallback((heatKey, value) => {
    setHeatColorByKey((prev) => ({
      ...prev,
      [heatKey]: value,
    }));
  }, []);

  // Get raw value for a given metric key from a row
  const getMetricValue = useCallback((row, metricKey) => {
    switch (metricKey) {
      case "wallTotalUs":
        return getWallUs(row);
      case "busyTotalUs":
        return row.est_cpu_us ?? row.cpu_time_us ?? null;
      case "waitTotalUs": {
        const wall = getWallUs(row);
        const cpu = row.est_cpu_us ?? row.cpu_time_us ?? null;
        if (wall == null || cpu == null) return null;
        return Math.max(0, wall - cpu);
      }
      case "selfWallUs":
        return getSelfWallUs(row);
      case "selfBusyUs":
        return getSelfCpuUs(row);
      case "selfWaitUs":
        return getSelfWaitUs(row);
      case "clarityRuntime":
        return typeof row.clarity_runtime_total === "number"
          ? row.clarity_runtime_total
          : typeof row.clarity_runtime === "number"
            ? row.clarity_runtime
            : null;
      default:
        return null;
    }
  }, []);

  const heatColumns = useMemo(() => {
    return COLUMN_DEFS.map((col) => {
      const heatKey = getHeatKeyForColumn(col);
      return heatKey ? { ...col, heatKey } : col;
    }).filter((col) => Boolean(col.heatKey));
  }, []);

  const getHeatValueForColumn = useCallback(
    (col, row) => {
      if (!col.heatKey) return null;
      const metricValue = getMetricValue(row, col.heatKey);
      if (Number.isFinite(metricValue)) return metricValue;
      const value = col.getter ? col.getter(row) : row[col.key];
      if (typeof value === "number" && Number.isFinite(value)) return value;
      if (typeof value === "string") {
        const parsed = Number(value);
        return Number.isFinite(parsed) ? parsed : null;
      }
      return null;
    },
    [getMetricValue]
  );

  const heatMax = useMemo(() => {
    const flat = flattenTree(withFlamePercent);
    const initial = {};
    for (const col of heatColumns) {
      if (col.heatKey && !(col.heatKey in initial)) initial[col.heatKey] = 0;
    }
    return flat.reduce((acc, row) => {
      for (const col of heatColumns) {
        const raw = getHeatValueForColumn(col, row);
        if (Number.isFinite(raw)) {
          acc[col.heatKey] = Math.max(acc[col.heatKey] ?? 0, raw);
        }
      }
      return acc;
    }, initial);
  }, [withFlamePercent, heatColumns, getHeatValueForColumn]);

  const getHeatBounds = useCallback(
    (heatKey) => {
      const config = heatConfig[heatKey] || {};
      const min = config.min ?? 0;
      const max = config.max ?? heatMax[heatKey] ?? 0;
      return { min, max, enabled: config.enabled === true };
    },
    [heatConfig, heatMax]
  );

  // Compute span viz value (level 0-1 and pct 0-100) for a row
  const getSpanVizValue = useCallback(
    (row) => {
      const metric = spanVizConfig.metric || "wallTotalUs";
      const raw = getMetricValue(row, metric);
      const max = heatMax[metric] ?? 0;
      const min = 0; // Always use 0 as min for span viz (auto scaling)

      if (raw == null || raw <= 0 || max <= min) {
        return { level: 0, pct: 0 };
      }

      const pct = Math.min(100, Math.max(0, ((raw - min) / (max - min)) * 100));
      const level = pct / 100;
      return { level, pct };
    },
    [spanVizConfig.metric, heatMax, getMetricValue]
  );

  const breadcrumb = focusedTree.breadcrumb ?? [];

  // Flat list of all rows for navigation in span details modal
  const allRowsFlat = useMemo(() => flattenTree(withFlamePercent), [withFlamePercent]);

  // Get the currently active span for the details modal
  const activeSpan = useMemo(() => {
    if (activeId == null) return null;
    return allRowsFlat.find((row) => row.id === activeId) || null;
  }, [activeId, allRowsFlat]);

  // Get the index of the active span in the flat list
  const activeSpanIndex = useMemo(() => {
    if (activeId == null) return -1;
    return allRowsFlat.findIndex((row) => row.id === activeId);
  }, [activeId, allRowsFlat]);

  // Navigation for span details modal
  const hasPreviousSpan = activeSpanIndex > 0;
  const hasNextSpan = activeSpanIndex >= 0 && activeSpanIndex < allRowsFlat.length - 1;

  const navigateToPreviousSpan = useCallback(() => {
    if (activeSpanIndex > 0) {
      setActiveId(allRowsFlat[activeSpanIndex - 1].id);
    }
  }, [activeSpanIndex, allRowsFlat]);

  const navigateToNextSpan = useCallback(() => {
    if (activeSpanIndex >= 0 && activeSpanIndex < allRowsFlat.length - 1) {
      setActiveId(allRowsFlat[activeSpanIndex + 1].id);
    }
  }, [activeSpanIndex, allRowsFlat]);

  useEffect(() => {
    setOpenNodes(computeDefaultOpenSet(focusedTree.roots, DEFAULT_AUTO_EXPAND));
  }, [rows, focusId, minWallFilterMs, focusedTree.roots]);

  const toggleChain = useCallback((rowId) => {
    setExpandedChains((prev) => {
      const next = new Set(prev);
      if (next.has(rowId)) {
        next.delete(rowId);
      } else {
        next.add(rowId);
      }
      return next;
    });
  }, []);

  const focusNode = useCallback((rowId) => {
    setFocusId(rowId);
    setActiveId(rowId);
  }, []);

  const clearFocus = useCallback(() => {
    setFocusId(null);
  }, []);

  const expandToDepth = useCallback(
    (depth) => {
      const openIds = new Set();
      const walk = (node, currentDepth) => {
        if (currentDepth < depth) openIds.add(node.id);
        if (currentDepth < depth) {
          (node.data || []).forEach((child) => walk(child, currentDepth + 1));
        }
      };
      focusedTree.roots.forEach((node) => walk(node, 0));
      setOpenNodes(openIds);
    },
    [focusedTree.roots]
  );

  const collapseSiblings = useCallback(() => {
    if (!activeId) return;
    const activeNode = prunedById.get(activeId);
    if (!activeNode || !activeNode.parent_id) return;
    const parent = prunedById.get(activeNode.parent_id);
    if (!parent) return;
    const next = new Set(openNodes);
    parent.data.forEach((child) => {
      if (child.id !== activeId) {
        const ids = new Set();
        collectSubtreeIds(child, ids);
        ids.forEach((id) => next.delete(id));
      }
    });
    next.add(parent.id);
    next.add(activeId);
    setOpenNodes(next);
  }, [activeId, prunedById, openNodes]);

  const buildHeatHeader = useMemo(
    () =>
      createHeatHeaderBuilder({
        heatConfig,
        toggleHeat,
        setHeatMin,
        setHeatMax,
        heatMenuOpenId,
        setHeatMenuOpenId,
        heatStyleByKey,
        setHeatStyleForKey,
        heatColorByKey,
        setHeatColorForKey,
        heatColorOptions: HEAT_COLOR_OPTIONS,
        minWallFilterMs,
        setMinWallFilterMs,
        defaultHeatStyle: DEFAULT_HEAT_STYLE,
        defaultHeatColor: DEFAULT_HEAT_COLOR,
      }),
    [
      heatConfig,
      heatMenuOpenId,
      heatStyleByKey,
      heatColorByKey,
      minWallFilterMs,
      setHeatMax,
      setHeatMin,
      setHeatStyleForKey,
      setHeatColorForKey,
      setHeatMenuOpenId,
      toggleHeat,
    ]
  );

  const buildSpanHeader = useMemo(
    () =>
      createSpanHeaderBuilder({
        spanVizConfig,
        setSpanVizConfig,
        spanVizMenuOpen,
        setSpanVizMenuOpen,
      }),
    [spanVizConfig, spanVizMenuOpen]
  );

  const visibleColumns = useMemo(() => {
    // Build 3-element header arrays for all columns
    // [level1Header, level2Header, level3Header]
    const build3LevelHeader = (col) => {
      // Level 1: Top group (Wall Time, Clarity, etc.) or empty for standalone
      const level1 = col.level1Start
        ? { text: col.level1, colspan: col.level1Span, css: "grid-group-header grid-level1-header" }
        : col.level1
          ? { text: "", _hidden: true }
          : { text: "", css: "grid-level1-empty" };
      
      // Level 2: Subgroup (Inclusive, Self, Runtime, etc.) or empty for standalone
      const level2 = col.level2Start
        ? { text: col.level2, colspan: col.level2Span, css: "grid-group-header grid-level2-header" }
        : col.level2
          ? { text: "", _hidden: true }
          : { text: "", css: "grid-level2-empty" };
      
      // Level 3: Column label (Total, Avg, or standalone label)
      const level3 = col.key === "span"
        ? buildSpanHeader(col)
        : col.heatKey
          ? buildHeatHeader(col)
          : { text: col.level3 || col.label };
      
      return [level1, level2, level3];
    };

    return COLUMN_DEFS.map((col) => {
      const heatKey = getHeatKeyForColumn(col);
      const columnDef = heatKey ? { ...col, heatKey } : col;
      const isNumeric = NUMERIC_COLUMN_KEYS.has(col.key);
      const baseWidth = col.width ?? (isNumeric ? DEFAULT_NUMERIC_WIDTH : undefined);
      const width = isNumeric && baseWidth != null
        ? Math.max(MIN_NUMERIC_WIDTH, baseWidth)
        : baseWidth;
      return {
      id: col.key,
      header: build3LevelHeader(columnDef),
      width,
      flexgrow: col.flexgrow,
      heatKey,
      treetoggle: col.treetoggle,
      cell:
        col.key === "span"
          ? (props) => (
              <SpanCell
                {...props}
                onToggleChain={toggleChain}
                onFocus={focusNode}
                spanVizConfig={spanVizConfig}
                getSpanVizValue={getSpanVizValue}
              />
            )
          : heatKey
            ? (props) => {
                const raw = getHeatValueForColumn(columnDef, props.row);
                const bounds = getHeatBounds(heatKey);
                const max = bounds.max;
                const min = bounds.min;
                const pct =
                  bounds.enabled && raw != null && raw > 0 && max > min
                    ? ((raw - min) / (max - min)) * 100
                    : 0;
                const value = col.getter ? col.getter(props.row) : props.row[col.key] ?? "-";
                const heatStyle = heatStyleByKey[heatKey] || DEFAULT_HEAT_STYLE;
                const heatColor = heatColorByKey[heatKey] || DEFAULT_HEAT_COLOR;
                return (
                  <HeatCell
                    row={props.row}
                    value={value}
                    percent={pct}
                    format={col.format}
                    numberFormat={numberFormat}
                    dimZero={COUNT_DIM_KEYS.has(col.key)}
                    heatStyle={heatStyle}
                    heatColor={heatColor}
                  />
                );
              }
            : NUMERIC_COLUMN_KEYS.has(col.key)
              ? (props) => {
                  const value = col.getter ? col.getter(props.row) : props.row[col.key] ?? "-";
                  return (
                    <NumericCell
                      row={props.row}
                      value={value}
                      format={col.format}
                      numberFormat={numberFormat}
                      dimZero={COUNT_DIM_KEYS.has(col.key)}
                    />
                  );
                }
              : col.cell,
      getter: col.getter,
      hidden: col.alwaysHidden
        ? true
        : col.alwaysVisible
          ? false
          : selectedColumns.includes(col.key)
            ? false
            : true,
      resize: true,
    };
    });
  }, [
    selectedColumns,
    toggleChain,
    focusNode,
    buildHeatHeader,
    buildSpanHeader,
    getHeatBounds,
    numberFormat,
    spanVizConfig,
    getSpanVizValue,
    heatColorByKey,
    heatStyleByKey,
    getHeatValueForColumn,
  ]);

  const toggleColumn = (key) => {
    setSelectedColumns((prev) => {
      if (ALWAYS_VISIBLE_KEYS.has(key) || ALWAYS_HIDDEN_KEYS.has(key)) return prev;
      const columnDef = COLUMN_DEFS.find((col) => col.key === key);
      if (!columnDef || columnDef.selectable === false) return prev;
      const next = prev.includes(key) ? prev.filter((k) => k !== key) : [...prev, key];
      const sanitized = sanitizeSelectedColumns(next);
      localStorage.setItem("profilerColumns", JSON.stringify(sanitized));
      return sanitized;
    });
  };

  const toggleColumnGroup = (keys, enable) => {
    setSelectedColumns((prev) => {
      let next = [...prev];
      for (const key of keys) {
        if (ALWAYS_VISIBLE_KEYS.has(key) || ALWAYS_HIDDEN_KEYS.has(key)) continue;
        const columnDef = COLUMN_DEFS.find((col) => col.key === key);
        if (!columnDef || columnDef.selectable === false) continue;
        if (enable && !next.includes(key)) {
          next.push(key);
        } else if (!enable && next.includes(key)) {
          next = next.filter((k) => k !== key);
        }
      }
      const sanitized = sanitizeSelectedColumns(next);
      localStorage.setItem("profilerColumns", JSON.stringify(sanitized));
      return sanitized;
    });
  };

  return (
    <div className={`app-container ${theme}`}>
      {/* Settings Panel */}
      <SettingsPanel
        open={settingsOpen}
        onClose={() => setSettingsOpen(false)}
        mode={mode}
        setMode={setMode}
        minWallMs={minWallMs}
        setMinWallMs={setMinWallMs}
        limit={limit}
        setLimit={setLimit}
        hotPathMode={hotPathMode}
        setHotPathMode={setHotPathMode}
        chainCompression={chainCompression}
        setChainCompression={setChainCompression}
        numberFormatId={numberFormatId}
        setNumberFormatId={setNumberFormatId}
        themePreset={themePreset}
        setThemePreset={setThemePreset}
        segmentRootId={segmentRootId}
        setSegmentRootId={setSegmentRootId}
        stacksBlockId={stacksBlockId}
        setStacksBlockId={setStacksBlockId}
        blocks={blocks}
        numberFormats={NUMBER_FORMATS}
        themePresets={THEME_PRESETS}
      />

      {/* Header */}
      <HeaderBar
        runs={runs}
        runId={runId}
        setRunId={setRunId}
        txQuery={txQuery}
        setTxQuery={setTxQuery}
        mode={mode}
        rowsLength={rows.length}
        theme={theme}
        toggleTheme={toggleTheme}
        resetQuery={resetQuery}
        onOpenSettings={() => setSettingsOpen(true)}
        loadTrace={loadTrace}
        cancelLoad={cancelLoad}
        isDirty={isDirty}
        isLoading={isLoading}
        activeTab={activeTab}
        setActiveTab={setTab}
      />

      {/* Trace View */}
      {activeTab === "trace" && (
        <>
          {/* Toolbar */}
          <ToolbarBar
            selectedColumns={selectedColumns}
        toggleColumn={toggleColumn}
        toggleColumnGroup={toggleColumnGroup}
        expandToDepth={expandToDepth}
        hotPathMode={hotPathMode}
        setHotPathMode={setHotPathMode}
        chainCompression={chainCompression}
        setChainCompression={setChainCompression}
        collapseSiblings={collapseSiblings}
        activeId={activeId}
        focusId={focusId}
        clearFocus={clearFocus}
        summary={summary}
      />

          {/* Breadcrumb */}
          <BreadcrumbBar breadcrumb={breadcrumb} />

          {/* Grid */}
          <ProfilerGrid
            data={roots}
            columns={visibleColumns}
            spanVizEnabled={spanVizConfig.enabled}
            spanVizStyle={spanVizConfig.style}
            isLoading={isLoading}
            isEmpty={rows.length === 0 && !isLoading}
            rowStyle={() => "profiler-row"}
            columnStyle={(column) =>
              NUMERIC_COLUMN_KEYS.has(column.id)
                ? "grid-col-numeric"
                : column.id === "span"
                  ? "grid-col-span"
                  : ""
            }
            onInit={(api) => {
              gridApiRef.current = api;
            }}
            onOpenRow={(ev) => {
              setOpenNodes((prev) => new Set(prev).add(ev.id));
            }}
            onCloseRow={(ev) => {
              setOpenNodes((prev) => {
                const next = new Set(prev);
                next.delete(ev.id);
                return next;
              });
            }}
            onSelectRow={(ev) => {
              if (ev?.id != null) {
                setActiveId(ev.id);
                setSpanDetailsOpen(true);
              }
            }}
          />
        </>
      )}

      {/* Transactions View */}
      {activeTab === "transactions" && (
        <TransactionsTab
          runId={runId}
          onViewTrace={viewTransactionTrace}
          numberFormat={numberFormat}
        />
      )}

      {/* Span Details Modal */}
      <SpanDetailsModal
        open={spanDetailsOpen}
        onOpenChange={setSpanDetailsOpen}
        span={activeSpan}
        onPrevious={navigateToPreviousSpan}
        onNext={navigateToNextSpan}
        hasPrevious={hasPreviousSpan}
        hasNext={hasNextSpan}
        numberFormat={numberFormat}
      />
    </div>
  );
}
