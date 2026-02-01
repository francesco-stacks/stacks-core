import React from "react";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { Sun, Moon, RotateCcw, Settings, Loader2, X, Square } from "lucide-react";

export default function HeaderBar({
  runs,
  runId,
  setRunId,
  txQuery,
  setTxQuery,
  mode,
  rowsLength,
  theme,
  toggleTheme,
  resetQuery,
  onOpenSettings,
  loadTrace,
  cancelLoad,
  isDirty,
  isLoading,
}) {
  const canSearch = txQuery.length === 64 && /^[0-9a-fA-F]{64}$/.test(txQuery);
  
  const handleKeyDown = (e) => {
    if (e.key === "Enter" && canSearch) {
      e.preventDefault();
      loadTrace();
    }
  };
  
  return (
    <header className="app-header">
      <div className="header-left">
        <h1 className="header-title">Profiler Explorer</h1>
        <div className="header-divider" />
        <Select value={runId} onValueChange={setRunId}>
          <SelectTrigger className="w-[200px]">
            <SelectValue placeholder="Select a run" />
          </SelectTrigger>
          <SelectContent>
            {runs.map((run) => (
              <SelectItem key={run.id} value={String(run.id)}>
                {run.run_name ? `${run.id} · ${run.run_name}` : `Run ${run.id}`}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>

      <div className="header-center">
        <div
          className={`search-container ${
            txQuery && !/^[0-9a-fA-F]{0,64}$/.test(txQuery)
              ? "search-container-invalid"
              : ""
          }`}
        >
          <svg
            className="search-icon"
            width="16"
            height="16"
            viewBox="0 0 16 16"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.5"
          >
            <circle cx="7" cy="7" r="5" />
            <path d="M11 11l3 3" />
          </svg>
          <input
            type="text"
            className="search-input"
            value={txQuery}
            onChange={(e) => {
              const value = e.target.value.trim();
              if (value === "" || /^[0-9a-fA-F]{0,64}$/.test(value)) {
                setTxQuery(value);
              }
            }}
            onKeyDown={handleKeyDown}
            placeholder={
              mode === "tx" ? "Enter transaction hash (64 hex chars)..." : "Search..."
            }
            maxLength={64}
            spellCheck={false}
          />
          {txQuery && (
            <>
              <span
                className={`search-length ${txQuery.length === 64 ? "search-length-valid" : ""}`}
              >
                {txQuery.length}/64
              </span>
              <Button
                variant="ghost"
                size="icon"
                className="h-6 w-6 mr-1"
                onClick={() => setTxQuery("")}
              >
                <X className="h-3 w-3" />
              </Button>
            </>
          )}
        </div>
      </div>

      <div className="header-right">
        <div className="header-stats">
          <span className="stat-badge">{rowsLength.toLocaleString()} rows</span>
        </div>
        <TooltipProvider delayDuration={300}>
          <Tooltip>
            <TooltipTrigger asChild>
              <Button variant="outline" size="icon" onClick={toggleTheme}>
                {theme === "dark" ? (
                  <Sun className="h-4 w-4" />
                ) : (
                  <Moon className="h-4 w-4" />
                )}
              </Button>
            </TooltipTrigger>
            <TooltipContent>
              {theme === "dark" ? "Switch to light mode" : "Switch to dark mode"}
            </TooltipContent>
          </Tooltip>
        </TooltipProvider>
        <TooltipProvider delayDuration={300}>
          <Tooltip>
            <TooltipTrigger asChild>
              <Button variant="outline" size="icon" onClick={resetQuery}>
                <RotateCcw className="h-4 w-4" />
              </Button>
            </TooltipTrigger>
            <TooltipContent>Reset all filters</TooltipContent>
          </Tooltip>
        </TooltipProvider>
        <TooltipProvider delayDuration={300}>
          <Tooltip>
            <TooltipTrigger asChild>
              <Button variant="outline" size="icon" onClick={onOpenSettings}>
                <Settings className="h-4 w-4" />
              </Button>
            </TooltipTrigger>
            <TooltipContent>Settings</TooltipContent>
          </Tooltip>
        </TooltipProvider>
        {isLoading ? (
          <Button variant="destructive" onClick={cancelLoad}>
            <Square className="h-4 w-4 mr-2" />
            Cancel
          </Button>
        ) : (
          <Button onClick={loadTrace} disabled={!isDirty}>
            Load Trace
          </Button>
        )}
      </div>
    </header>
  );
}
