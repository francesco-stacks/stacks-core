import React from "react";

const SPAN_VIZ_METRICS = [
  { key: "wallTotalUs", label: "Wall Total" },
  { key: "selfWallUs", label: "Wall Self" },
  { key: "busyTotalUs", label: "Busy Total" },
  { key: "selfBusyUs", label: "Busy Self" },
  { key: "waitTotalUs", label: "Wait Total" },
  { key: "selfWaitUs", label: "Wait Self" },
  { key: "clarityRuntime", label: "Clarity Runtime" },
];

export default function SpanHeaderCell({ cell, spanVizConfig, setSpanVizConfig, isOpen, setIsOpen }) {
  return (
    <div className="span-header">
      <span>{cell.text}</span>
      <button
        type="button"
        className="span-header-btn"
        onClick={(event) => {
          event.stopPropagation();
          setIsOpen(!isOpen);
        }}
      >
        ⚙
      </button>
      {isOpen ? (
        <div
          className="span-viz-menu"
          onClick={(event) => {
            event.stopPropagation();
          }}
        >
          <label>
            <input
              type="checkbox"
              checked={spanVizConfig.enabled}
              onChange={() =>
                setSpanVizConfig((prev) => ({ ...prev, enabled: !prev.enabled }))
              }
            />
            Show Span Viz
          </label>
          <label>
            Style
            <select
              className="span-viz-select"
              value={spanVizConfig.style}
              onChange={(event) =>
                setSpanVizConfig((prev) => ({ ...prev, style: event.target.value }))
              }
            >
              <option value="fill">Fill</option>
              <option value="edge">Edge</option>
              <option value="meter">Meter</option>
            </select>
          </label>
          <label>
            Metric
            <select
              className="span-viz-select"
              value={spanVizConfig.metric}
              onChange={(event) =>
                setSpanVizConfig((prev) => ({ ...prev, metric: event.target.value }))
              }
            >
              {SPAN_VIZ_METRICS.map((m) => (
                <option key={m.key} value={m.key}>
                  {m.label}
                </option>
              ))}
            </select>
          </label>
        </div>
      ) : null}
    </div>
  );
}
