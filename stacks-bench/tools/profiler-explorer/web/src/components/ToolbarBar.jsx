import React from "react";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Columns3 } from "lucide-react";

function ColumnDropdown({ columns, selected, onChange }) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button variant="outline" size="sm" className="gap-2">
          <Columns3 className="h-4 w-4" />
          Columns
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="max-h-80 w-56 overflow-auto">
        <DropdownMenuLabel>Visible Columns</DropdownMenuLabel>
        <DropdownMenuSeparator />
        {columns.map((col) => (
          <DropdownMenuCheckboxItem
            key={col.key}
            checked={selected.includes(col.key)}
            onCheckedChange={() => onChange(col.key)}
          >
            {col.label}
          </DropdownMenuCheckboxItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

export default function ToolbarBar({
  columns,
  selectedColumns,
  toggleColumn,
  expandToDepth,
  hotPathMode,
  collapseSiblings,
  activeId,
  focusId,
  clearFocus,
  summary,
}) {
  return (
    <div className="app-toolbar">
      <div className="toolbar-left">
        <ColumnDropdown
          columns={columns}
          selected={selectedColumns}
          onChange={toggleColumn}
        />
        <div className="toolbar-divider" />
        <div className="toolbar-group">
          <span className="toolbar-label">Depth:</span>
          {[2, 4, 6].map((depth) => (
            <Button
              key={depth}
              variant="outline"
              size="sm"
              onClick={() => expandToDepth(depth)}
              disabled={hotPathMode !== "off"}
            >
              {depth}
            </Button>
          ))}
        </div>
        <Button
          variant="outline"
          size="sm"
          onClick={collapseSiblings}
          disabled={!activeId}
        >
          Collapse siblings
        </Button>
        {focusId && (
          <Button
            variant="outline"
            size="sm"
            onClick={clearFocus}
            className="border-primary text-primary hover:bg-primary/10"
          >
            Clear focus
          </Button>
        )}
      </div>
      <div className="toolbar-right">
        {summary && <span className="toolbar-status">{summary}</span>}
      </div>
    </div>
  );
}
