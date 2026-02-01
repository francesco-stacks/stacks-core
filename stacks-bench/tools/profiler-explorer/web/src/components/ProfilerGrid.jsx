import React from "react";
import { Grid } from "@svar-ui/react-grid";
import { WillowDark } from "@svar-ui/react-core";
import { Loader2 } from "lucide-react";

export default function ProfilerGrid({
  data,
  columns,
  spanVizEnabled,
  spanVizStyle,
  isLoading,
  isEmpty,
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
      {isLoading && (
        <div className="grid-loading-overlay">
          <Loader2 className="h-8 w-8 animate-spin text-muted-foreground" />
          <span className="text-sm text-muted-foreground mt-2">Loading trace data...</span>
        </div>
      )}
      {isEmpty && !isLoading && (
        <div className="grid-empty-state">
          <span className="text-sm text-muted-foreground">No data to display. Enter a search query above.</span>
        </div>
      )}
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
