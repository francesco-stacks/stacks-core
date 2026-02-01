import React from "react";
import HeatHeaderCell from "./components/HeatHeaderCell";
import SpanHeaderCell from "./components/SpanHeaderCell";

export function createHeatHeaderBuilder({
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
  heatColorOptions,
  minWallFilterMs,
  setMinWallFilterMs,
  defaultHeatStyle,
  defaultHeatColor,
}) {
  return (col) => ({
    text: col.headerLabel ?? col.label,
    cell: (props) => (
      <HeatHeaderCell
        {...props}
        heatConfig={heatConfig}
        onToggle={toggleHeat}
        onMinChange={setHeatMin}
        onMaxChange={setHeatMax}
        openId={heatMenuOpenId}
        setOpenId={setHeatMenuOpenId}
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

export function createSpanHeaderBuilder({
  spanVizConfig,
  setSpanVizConfig,
  spanVizMenuOpen,
  setSpanVizMenuOpen,
}) {
  return (col) => ({
    text: col.label,
    cell: (props) => (
      <SpanHeaderCell
        {...props}
        spanVizConfig={spanVizConfig}
        setSpanVizConfig={setSpanVizConfig}
        isOpen={spanVizMenuOpen}
        setIsOpen={setSpanVizMenuOpen}
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
