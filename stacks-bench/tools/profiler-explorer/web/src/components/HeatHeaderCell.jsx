import React from "react";

export default function HeatHeaderCell({
  column,
  cell,
  heatConfig,
  onToggle,
  onMinChange,
  onMaxChange,
  openId,
  setOpenId,
  heatStyle,
  setHeatStyle,
  heatColor,
  setHeatColor,
  heatColorOptions,
  minWallFilterMs,
  setMinWallFilterMs,
}) {
  const heatKey = column.heatKey;
  const isOpen = openId === column.id;
  if (!heatKey) {
    return <div className="heat-header">{cell.text}</div>;
  }
  const config = heatConfig[heatKey] || { enabled: true, min: null, max: null };
  return (
    <div className="heat-header">
      <span>{cell.text}</span>
      <button
        type="button"
        className="heat-header-btn"
        onClick={(event) => {
          event.stopPropagation();
          setOpenId(isOpen ? null : column.id);
        }}
      >
        ⚙
      </button>
      {isOpen ? (
        <div
          className="heat-menu"
          onClick={(event) => {
            event.stopPropagation();
          }}
        >
          <label>
            <input
              type="checkbox"
              checked={config.enabled}
              onChange={() => onToggle(heatKey)}
            />
            Heatmap enabled
          </label>
          <label>
            Min
            <input
              type="number"
              placeholder="auto"
              value={config.min ?? ""}
              onChange={(event) => onMinChange(heatKey, event.target.value)}
            />
          </label>
          <label>
            Max
            <input
              type="number"
              placeholder="auto"
              value={config.max ?? ""}
              onChange={(event) => onMaxChange(heatKey, event.target.value)}
            />
          </label>
          <label>
            Heat Style
            <select
              className="heat-style-select"
              value={heatStyle}
              onChange={(event) => setHeatStyle(heatKey, event.target.value)}
            >
              <option value="fill">Fill</option>
              <option value="edge">Edge</option>
              <option value="meter">Meter</option>
            </select>
          </label>
          <label>
            Heat Color
            <select
              className="heat-style-select"
              value={heatColor}
              onChange={(event) => setHeatColor(heatKey, event.target.value)}
            >
              {heatColorOptions.map((option) => (
                <option key={option.id} value={option.id}>
                  {option.label}
                </option>
              ))}
            </select>
          </label>
          {column.id === "wall_total" ? (
            <label>
              Min (table ms)
              <input
                type="number"
                placeholder="off"
                value={minWallFilterMs}
                onChange={(event) => setMinWallFilterMs(event.target.value)}
              />
            </label>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}
