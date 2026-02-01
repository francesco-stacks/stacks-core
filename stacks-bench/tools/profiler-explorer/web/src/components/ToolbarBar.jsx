import React, { useMemo } from "react";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Columns3, Flame, Link2, ChevronRight } from "lucide-react";

function ColumnDropdown({ columns, selected, onChange, onToggleGroup }) {
  // Build hierarchical structure: ungrouped columns + groups with children
  const { ungrouped, groups } = useMemo(() => {
    const ungrouped = [];
    const groupMap = new Map();
    
    for (const col of columns) {
      if (col.group) {
        if (!groupMap.has(col.group)) {
          groupMap.set(col.group, []);
        }
        groupMap.get(col.group).push(col);
      } else {
        ungrouped.push(col);
      }
    }
    
    const groups = Array.from(groupMap.entries()).map(([name, cols]) => ({
      name,
      columns: cols,
    }));
    
    return { ungrouped, groups };
  }, [columns]);

  const isGroupFullySelected = (group) => 
    group.columns.every((col) => selected.includes(col.key));
  
  const isGroupPartiallySelected = (group) =>
    group.columns.some((col) => selected.includes(col.key)) && 
    !isGroupFullySelected(group);

  const handleGroupToggle = (group) => {
    const keys = group.columns.map((c) => c.key);
    onToggleGroup(keys, !isGroupFullySelected(group));
  };

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button variant="outline" size="sm" className="gap-2">
          <Columns3 className="h-4 w-4" />
          Columns
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="max-h-96 w-64 overflow-auto">
        <DropdownMenuLabel>Visible Columns</DropdownMenuLabel>
        <DropdownMenuSeparator />
        
        {/* Ungrouped columns */}
        {ungrouped.map((col) => (
          <DropdownMenuCheckboxItem
            key={col.key}
            checked={selected.includes(col.key)}
            onCheckedChange={() => onChange(col.key)}
            onSelect={(e) => e.preventDefault()}
          >
            {col.label}
          </DropdownMenuCheckboxItem>
        ))}
        
        {ungrouped.length > 0 && groups.length > 0 && <DropdownMenuSeparator />}
        
        {/* Grouped columns */}
        {groups.map((group) => (
          <div key={group.name} className="column-group">
            <DropdownMenuCheckboxItem
              checked={isGroupFullySelected(group)}
              className="font-medium"
              onCheckedChange={() => handleGroupToggle(group)}
              onSelect={(e) => e.preventDefault()}
            >
              <span className="flex items-center gap-1">
                <ChevronRight className={`h-3 w-3 transition-transform ${isGroupPartiallySelected(group) || isGroupFullySelected(group) ? "rotate-90" : ""}`} />
                {group.name}
              </span>
            </DropdownMenuCheckboxItem>
            <div className="column-group-children">
              {group.columns.map((col) => (
                <DropdownMenuCheckboxItem
                  key={col.key}
                  checked={selected.includes(col.key)}
                  onCheckedChange={() => onChange(col.key)}
                  onSelect={(e) => e.preventDefault()}
                  className="pl-6 text-muted-foreground text-xs"
                >
                  {col.headerLabel || col.label.replace(/ \(ms\)$/, "").replace(group.name.replace(" (ms)", "") + " ", "")}
                </DropdownMenuCheckboxItem>
              ))}
            </div>
          </div>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function HotPathDropdown({ hotPathMode, setHotPathMode }) {
  const isActive = hotPathMode !== "off";
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          variant={isActive ? "default" : "outline"}
          size="sm"
          className={`gap-2 ${isActive ? "bg-orange-600 hover:bg-orange-700 text-white" : ""}`}
        >
          <Flame className="h-4 w-4" />
          Hot Path
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start">
        <DropdownMenuLabel>Hot Path Mode</DropdownMenuLabel>
        <DropdownMenuSeparator />
        <DropdownMenuCheckboxItem
          checked={hotPathMode === "off"}
          onCheckedChange={() => setHotPathMode("off")}
        >
          Off
        </DropdownMenuCheckboxItem>
        <DropdownMenuCheckboxItem
          checked={hotPathMode === "inclusive"}
          onCheckedChange={() => setHotPathMode("inclusive")}
        >
          Inclusive Time
        </DropdownMenuCheckboxItem>
        <DropdownMenuCheckboxItem
          checked={hotPathMode === "self"}
          onCheckedChange={() => setHotPathMode("self")}
        >
          Self Time
        </DropdownMenuCheckboxItem>
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
  setHotPathMode,
  chainCompression,
  setChainCompression,
  collapseSiblings,
  activeId,
  focusId,
  clearFocus,
  summary,
  toggleColumnGroup,
}) {
  return (
    <div className="app-toolbar">
      <div className="toolbar-left">
        <ColumnDropdown
          columns={columns}
          selected={selectedColumns}
          onChange={toggleColumn}
          onToggleGroup={toggleColumnGroup}
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
        <div className="toolbar-divider" />
        <HotPathDropdown
          hotPathMode={hotPathMode}
          setHotPathMode={setHotPathMode}
        />
        <Button
          variant={chainCompression ? "default" : "outline"}
          size="sm"
          className={`gap-2 ${chainCompression ? "bg-emerald-600 hover:bg-emerald-700 text-white" : ""}`}
          onClick={() => setChainCompression(!chainCompression)}
        >
          <Link2 className="h-4 w-4" />
          Compress Chains
        </Button>
      </div>
      <div className="toolbar-right">
        {summary && <span className="toolbar-status">{summary}</span>}
      </div>
    </div>
  );
}
