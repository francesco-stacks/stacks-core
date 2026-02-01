import React from "react";
import { Grid } from "@svar-ui/react-grid";
import { WillowDark } from "@svar-ui/react-core";

export default function ProfilerGrid({
  data,
  columns,
  spanVizEnabled,
  spanVizStyle,
  rowStyle,
  columnStyle,
  onOpenRow,
  onCloseRow,
  onSelectRow,
  onInit,
}) {
  return (
    <section
      className={`app-grid-container${spanVizEnabled ? ` span-viz-${spanVizStyle}` : ""}`}
    >
      <WillowDark>
        <Grid
          tree={true}
          data={data}
          columns={columns}
          sizes={{ rowHeight: 36 }}
          select={true}
          rowStyle={rowStyle}
          columnStyle={columnStyle}
          init={onInit}
          onOpenRow={onOpenRow}
          onCloseRow={onCloseRow}
          onSelectRow={onSelectRow}
        />
      </WillowDark>
    </section>
  );
}
