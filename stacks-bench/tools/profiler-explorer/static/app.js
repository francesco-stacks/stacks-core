const runSelect = document.getElementById("runSelect");
const txSelect = document.getElementById("txSelect");
const txSearch = document.getElementById("txSearch");
const blockSelect = document.getElementById("blockSelect");
const minWallInput = document.getElementById("minWall");
const segmentRootInput = document.getElementById("segmentRoot");
const limitInput = document.getElementById("limit");
const loadTraceButton = document.getElementById("loadTrace");
const summary = document.getElementById("summary");
const treeContainer = document.getElementById("treeContainer");
const txSelectGroup = document.getElementById("txSelectGroup");
const blockSelectGroup = document.getElementById("blockSelectGroup");
const columnMenu = document.getElementById("columnMenu");
const columnToggle = document.getElementById("columnToggle");

const COLUMN_DEFS = [
  {
    key: "span",
    label: "Span",
    width: "2.4fr",
    default: true,
    render: (node, depth, hasChildren, isCollapsed) => {
      const container = document.createElement("div");
      container.className = "tree-indent";
      container.style.paddingLeft = `${depth * 14}px`;
      if (hasChildren) {
        const toggle = document.createElement("span");
        toggle.className = "tree-toggle";
        toggle.textContent = isCollapsed ? "▸" : "▾";
        toggle.dataset.toggleId = node.id;
        container.appendChild(toggle);
      } else {
        const spacer = document.createElement("span");
        spacer.style.width = "12px";
        container.appendChild(spacer);
      }
      const spanCell = document.createElement("div");
      spanCell.className = "span-cell";

      const track = document.createElement("div");
      track.className = "flame-track";
      const bar = document.createElement("div");
      bar.className = "flame-bar";
      const wallUs = getFlameWallUs(node);
      let percent = null;
      if (currentMaxWallUs > 0 && wallUs !== null) {
        percent = (wallUs / currentMaxWallUs) * 100;
        bar.style.width = `${Math.max(4, Math.round(percent))}%`;
      }
      track.appendChild(bar);

      const label = document.createElement("div");
      label.className = "span-label";
      if (percent !== null) {
        const pill = document.createElement("span");
        pill.className = "span-percent";
        pill.textContent = `${percent.toFixed(1)}%`;
        label.appendChild(pill);
      }
      const title = document.createElement("span");
      title.className = "span-name";
      title.textContent = node.span_name ?? "-";
      label.appendChild(title);
      if (node.tag) {
        const tag = document.createElement("span");
        tag.className = "span-tag";
        tag.textContent = node.tag;
        label.appendChild(tag);
      }

      spanCell.appendChild(track);
      spanCell.appendChild(label);
      container.appendChild(spanCell);
      return container;
    },
  },
  {
    key: "tag",
    label: "Tag",
    width: "1.1fr",
    default: false,
    render: (node) => node.tag ?? "-",
  },
  {
    key: "wall_total",
    label: "Wall Total (ms)",
    width: "1fr",
    default: true,
    render: (node) => formatMs(node.est_wall_us ?? node.wall_time_us),
  },
  {
    key: "wall_self",
    label: "Wall Self (ms)",
    width: "1fr",
    default: true,
    render: (node) => formatMs(node.est_self_wall_us ?? node.self_wall_time_us),
  },
  {
    key: "busy_total",
    label: "Busy Total (ms)",
    width: "1fr",
    default: false,
    render: (node) => formatMs(node.est_cpu_us ?? node.cpu_time_us),
  },
  {
    key: "wait_total",
    label: "Wait Total (ms)",
    width: "1fr",
    default: false,
    render: (node) => formatMs(calcWaitUs(node)),
  },
  {
    key: "calls",
    label: "Calls",
    width: "0.7fr",
    default: true,
    render: (node) => formatCount(node.call_count),
  },
  {
    key: "samples",
    label: "Samples",
    width: "0.7fr",
    default: true,
    render: (node) => formatCount(node.sample_count),
  },
  {
    key: "kv_total",
    label: "KV Total",
    width: "0.8fr",
    default: true,
    render: (node) => formatCount(node.kv_total),
  },
  {
    key: "clarity_rw",
    label: "Clarity R/W",
    width: "1fr",
    default: true,
    render: (node) => `${formatCount(node.clarity_read_count)}/${formatCount(node.clarity_write_count)}`,
  },
  {
    key: "clarity_len",
    label: "Clarity RL/WL",
    width: "1fr",
    default: false,
    render: (node) => `${formatCount(node.clarity_read_length)}/${formatCount(node.clarity_write_length)}`,
  },
  {
    key: "clarity_runtime",
    label: "Clarity Runtime",
    width: "1fr",
    default: true,
    render: (node) => formatCount(node.clarity_runtime),
  },
  {
    key: "record_id",
    label: "Record ID",
    width: "0.8fr",
    default: false,
    render: (node) => node.id,
  },
];

const collapsedNodes = new Set();
let currentNodes = [];
let currentMaxWallUs = 0;

function getMode() {
  return document.querySelector("input[name='mode']:checked").value;
}

async function fetchJson(url) {
  const res = await fetch(url);
  if (!res.ok) {
    const msg = await res.text();
    throw new Error(msg || `HTTP ${res.status}`);
  }
  return res.json();
}

function formatMs(value) {
  if (value === null || value === undefined) return "-";
  return (value / 1000.0).toFixed(3);
}

function formatCount(value) {
  if (value === null || value === undefined) return "-";
  return value;
}

function setSummary(text) {
  summary.textContent = text;
}

function calcWaitUs(node) {
  const wall = getDisplayWallUs(node);
  const cpu = node.est_cpu_us ?? node.cpu_time_us;
  if (wall === null || wall === undefined || cpu === null || cpu === undefined) return null;
  return Math.max(0, wall - cpu);
}

function getDisplayWallUs(node) {
  const wall = node.est_wall_us ?? node.wall_time_us;
  return wall === undefined ? null : wall;
}

function getFlameWallUs(node) {
  const wall = node.wall_time_us ?? node.est_wall_us;
  return wall === undefined ? null : wall;
}

function readMinWallUs() {
  const value = minWallInput.value;
  if (!value) return null;
  const parsed = Number(value);
  if (Number.isNaN(parsed)) return null;
  return parsed * 1000;
}

function getSelectedColumns() {
  const selected = new Set(
    Array.from(columnMenu.querySelectorAll("input[type='checkbox']:checked")).map(
      (input) => input.value,
    ),
  );
  return COLUMN_DEFS.filter((col) => selected.has(col.key));
}

function buildColumnSelector() {
  columnMenu.innerHTML = "";
  const saved = new Set(JSON.parse(localStorage.getItem("profilerColumns") || "[]"));
  for (const col of COLUMN_DEFS) {
    const label = document.createElement("label");
    const checkbox = document.createElement("input");
    checkbox.type = "checkbox";
    checkbox.value = col.key;
    checkbox.checked = saved.size ? saved.has(col.key) : col.default;
    checkbox.addEventListener("change", () => {
      const selected = getSelectedColumns().map((c) => c.key);
      localStorage.setItem("profilerColumns", JSON.stringify(selected));
      renderTree(currentNodes);
    });
    label.appendChild(checkbox);
    label.appendChild(document.createTextNode(col.label));
    columnMenu.appendChild(label);
  }
}

async function loadRuns() {
  const runs = await fetchJson("/api/runs");
  runSelect.innerHTML = "";
  for (const run of runs) {
    const option = document.createElement("option");
    option.value = run.id;
    const label = run.run_name ? `${run.id} · ${run.run_name}` : `Run ${run.id}`;
    const start = run.start_time ? ` · ${run.start_time}` : "";
    option.textContent = label + start;
    runSelect.appendChild(option);
  }
  if (runs.length > 0) {
    await loadBlocks();
    await loadTxs();
  }
}

async function loadBlocks() {
  const runId = runSelect.value;
  blockSelect.innerHTML = '<option value="">Any</option>';
  if (!runId) return;
  const blocks = await fetchJson(`/api/blocks?run_id=${runId}`);
  for (const block of blocks) {
    const option = document.createElement("option");
    option.value = block.stacks_block_id;
    option.textContent = `#${block.height} · ${block.block_hash_hex}`;
    blockSelect.appendChild(option);
  }
}

async function loadTxs() {
  const runId = runSelect.value;
  txSelect.innerHTML = "";
  if (!runId) return;
  const q = txSearch.value.trim();
  const url = new URL("/api/txs", window.location.origin);
  url.searchParams.set("run_id", runId);
  if (q) url.searchParams.set("q", q);
  const txs = await fetchJson(url.toString());
  for (const tx of txs) {
    const option = document.createElement("option");
    option.value = tx.stacks_tx_id;
    option.textContent = tx.tx_hash_hex;
    txSelect.appendChild(option);
  }
}

function buildTree(nodes) {
  const byId = new Map();
  const roots = [];
  for (const node of nodes) {
    node.children = [];
    byId.set(node.id, node);
  }
  for (const node of nodes) {
    if (node.parent_id && byId.has(node.parent_id)) {
      byId.get(node.parent_id).children.push(node);
    } else {
      roots.push(node);
    }
  }
  const sortByPath = (a, b) => (a.sort_path || "").localeCompare(b.sort_path || "");
  roots.sort(sortByPath);
  for (const node of nodes) {
    node.children.sort(sortByPath);
  }
  return roots;
}

function pruneTree(nodes, minWallUs) {
  if (!minWallUs) return nodes;
  const keepNodes = (node) => {
    const wall = getDisplayWallUs(node);
    const keptChildren = [];
    for (const child of node.children) {
      if (keepNodes(child)) {
        keptChildren.push(child);
      }
    }
    node.children = keptChildren;
    return (wall !== null && wall >= minWallUs) || keptChildren.length > 0;
  };

  const filtered = [];
  for (const node of nodes) {
    if (keepNodes(node)) {
      filtered.push(node);
    }
  }
  return filtered;
}

function renderTree(nodes) {
  treeContainer.innerHTML = "";
  currentNodes = nodes;
  currentMaxWallUs = nodes.reduce((maxVal, node) => {
    const wall = getFlameWallUs(node);
    return wall !== null && wall !== undefined ? Math.max(maxVal, wall) : maxVal;
  }, 0);
  const roots = pruneTree(buildTree(nodes), readMinWallUs());
  if (roots.length === 0) {
    treeContainer.textContent = "No records returned.";
    return;
  }

  const selectedColumns = getSelectedColumns();
  const gridTemplate = selectedColumns.map((col) => col.width).join(" ");

  const table = document.createElement("div");
  table.className = "tree-table";

  const header = document.createElement("div");
  header.className = "tree-table-header";
  header.style.gridTemplateColumns = gridTemplate;
  for (const col of selectedColumns) {
    const cell = document.createElement("div");
    cell.textContent = col.label;
    header.appendChild(cell);
  }
  table.appendChild(header);

  const body = document.createElement("div");

  const renderRows = (node, depth) => {
    const row = document.createElement("div");
    row.className = "tree-row";
    row.style.gridTemplateColumns = gridTemplate;
    const isCollapsed = collapsedNodes.has(node.id);
    for (const col of selectedColumns) {
      const cell = document.createElement("div");
      cell.className = "tree-cell";
      const value = col.render(node, depth, node.children.length > 0, isCollapsed);
      if (value instanceof HTMLElement) {
        cell.appendChild(value);
      } else {
        const span = document.createElement("span");
        span.textContent = value;
        cell.appendChild(span);
      }
      row.appendChild(cell);
    }
    body.appendChild(row);

    if (!isCollapsed) {
      for (const child of node.children) {
        renderRows(child, depth + 1);
      }
    }
  };

  for (const root of roots) {
    renderRows(root, 0);
  }

  body.addEventListener("click", (event) => {
    const target = event.target;
    if (!(target instanceof HTMLElement)) return;
    const toggle = target.closest("[data-toggle-id]");
    if (!toggle) return;
    const nodeId = Number(toggle.dataset.toggleId);
    if (collapsedNodes.has(nodeId)) {
      collapsedNodes.delete(nodeId);
    } else {
      collapsedNodes.add(nodeId);
    }
    renderTree(currentNodes);
  });

  table.appendChild(body);
  treeContainer.appendChild(table);
}

async function loadTrace() {
  const runId = runSelect.value;
  const mode = getMode();
  const limit = limitInput.value || 5000;
  const url = new URL("/api/trace", window.location.origin);
  url.searchParams.set("run_id", runId);
  url.searchParams.set("mode", mode);
  url.searchParams.set("limit", limit);

  if (mode === "tx") {
    if (txSelect.value) {
      url.searchParams.set("stacks_tx_id", txSelect.value);
    }
  } else {
    if (blockSelect.value) {
      url.searchParams.set("stacks_block_id", blockSelect.value);
    }
    if (segmentRootInput.value) {
      url.searchParams.set("segment_root_id", segmentRootInput.value);
    }
    if (minWallInput.value) {
      url.searchParams.set("min_wall_ms", minWallInput.value);
    }
  }

  setSummary("Loading trace...");
  const nodes = await fetchJson(url.toString());
  setSummary(`${nodes.length} records loaded.`);
  renderTree(nodes);
}

function updateModeUI() {
  const mode = getMode();
  txSelectGroup.style.display = mode === "tx" ? "flex" : "none";
  blockSelectGroup.style.display = mode === "run" ? "flex" : "none";
}

runSelect.addEventListener("change", async () => {
  await loadBlocks();
  await loadTxs();
});

txSearch.addEventListener("input", async () => {
  await loadTxs();
});

loadTraceButton.addEventListener("click", async () => {
  try {
    await loadTrace();
  } catch (err) {
    setSummary(`Error: ${err.message}`);
  }
});

columnToggle.addEventListener("click", () => {
  columnMenu.classList.toggle("open");
});

document.addEventListener("click", (event) => {
  if (!columnMenu.classList.contains("open")) return;
  const target = event.target;
  if (!(target instanceof HTMLElement)) return;
  if (target.closest(".dropdown")) return;
  columnMenu.classList.remove("open");
});

for (const radio of document.querySelectorAll("input[name='mode']")) {
  radio.addEventListener("change", updateModeUI);
}

updateModeUI();
buildColumnSelector();
loadRuns().catch((err) => setSummary(`Error: ${err.message}`));
