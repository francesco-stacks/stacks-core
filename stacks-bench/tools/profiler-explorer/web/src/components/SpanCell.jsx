import React, { useEffect, useRef } from "react";
import { useProfilerGridContext } from "../contexts/ProfilerGridContext";

export default function SpanCell({ row }) {
  const { toggleChain, focusNode, spanVizConfig, getSpanVizValue } = useProfilerGridContext();
  const cellRef = useRef(null);
  const percent = row.flame_percent ?? 0;
  const label = row.chain_label ?? row.span_name ?? "-";
  const chainCount = row.chain_count ?? 0;
  const hiddenSiblings = row.hidden_siblings ?? 0;
  const tooltipLines = [];
  if (Array.isArray(row.chain_segments) && row.chain_segments.length > 0) {
    row.chain_segments.forEach((segment, index) => {
      const prefix = segment.tag ? `(${segment.tag}): ` : "";
      tooltipLines.push(`${index + 1}. ${prefix}${segment.name}`);
    });
  } else if (row.span_name) {
    const prefix = row.tag ? `(${row.tag}): ` : "";
    tooltipLines.push(`1. ${prefix}${row.span_name}`);
  }
  const tooltip = tooltipLines.join("\n");

  useEffect(() => {
    if (!cellRef.current) return;
    const wxCell = cellRef.current.closest(".wx-cell");
    if (!wxCell) return;

    if (!spanVizConfig?.enabled) {
      wxCell.style.setProperty("--span-viz-width", "0");
      wxCell.style.setProperty("--span-viz-alpha", "0");
      return;
    }

    const { level, pct } = getSpanVizValue?.(row) ?? { level: 0, pct: 0 };
    const alpha = level > 0 ? 0.04 + level * 0.2 : 0;

    wxCell.style.setProperty("--span-viz-width", String(pct));
    wxCell.style.setProperty("--span-viz-alpha", String(alpha));
  }, [row, spanVizConfig, getSpanVizValue]);

  return (
    <div className="span-cell" title={tooltip} ref={cellRef}>
      <div className="span-label">
        <span className="span-percent">{percent.toFixed(1)}%</span>
        <span className="span-name">{label}</span>
        {row.tag ? <span className="span-tag">{row.tag}</span> : null}
        {chainCount > 0 ? (
          <button
            type="button"
            className="span-muted-badge"
            onClick={(event) => {
              event.stopPropagation();
              toggleChain?.(row.id);
            }}
          >
            +{chainCount} frames
          </button>
        ) : null}
        {hiddenSiblings > 0 ? (
          <span className="span-muted-badge">
            +{hiddenSiblings} siblings
          </span>
        ) : null}
        <button
          type="button"
          className="span-focus-btn"
          onClick={(event) => {
            event.stopPropagation();
            focusNode?.(row.id);
          }}
        >
          Focus
        </button>
      </div>
    </div>
  );
}
