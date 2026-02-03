import React from "react";
import HeatHeaderCell from "./components/HeatHeaderCell";
import SpanHeaderCell from "./components/SpanHeaderCell";

/**
 * Creates a heat header builder function.
 * 
 * NOTE: The openId/setOpenId props are intentionally NOT included here.
 * They are managed via HeatHeaderMenuContext to avoid recreating column
 * definitions every time a menu is opened/closed.
 */
export function createHeatHeaderBuilder({
  heatConfig,
  toggleHeat,
  setHeatMin,
  setHeatMax,
  heatStyleByKey,
  setHeatStyleForKey,
  heatColorByKey,
  setHeatColorForKey,
  heatColorOptions,
  minWallFilterMs,
  setMinWallFilterMs,
  defaultHeatStyle,
  defaultHeatColor,
}) {
  return (col) => ({
    text: col.level3 || col.headerLabel || col.label,
    cell: (props) => (
      <HeatHeaderCell
        {...props}
        heatConfig={heatConfig}
        onToggle={toggleHeat}
        onMinChange={setHeatMin}
        onMaxChange={setHeatMax}
        heatStyle={heatStyleByKey[col.heatKey] || defaultHeatStyle}
        setHeatStyle={setHeatStyleForKey}
        heatColor={heatColorByKey[col.heatKey] || defaultHeatColor}
        setHeatColor={setHeatColorForKey}
        heatColorOptions={heatColorOptions}
        minWallFilterMs={minWallFilterMs}
        setMinWallFilterMs={setMinWallFilterMs}
      />
    ),
  });
}

/**
 * Creates a span header builder function.
 * 
 * NOTE: The isOpen/setIsOpen props are intentionally NOT included here.
 * They are managed via SpanHeaderMenuContext to avoid recreating column
 * definitions every time the menu is opened/closed.
 */
export function createSpanHeaderBuilder({
  spanVizConfig,
  setSpanVizConfig,
}) {
  return (col) => ({
    text: col.label,
    cell: (props) => (
      <SpanHeaderCell
        {...props}
        spanVizConfig={spanVizConfig}
        setSpanVizConfig={setSpanVizConfig}
      />
    ),
  });
}

export function createGroupHeaderBuilder(buildHeatHeader) {
  return (col) => [
    col.groupStart
      ? { text: col.group, colspan: col.groupSpan, css: "grid-group-header" }
      : { text: "", _hidden: true },
    col.heatKey ? buildHeatHeader(col) : { text: col.headerLabel ?? col.label },
  ];
}
