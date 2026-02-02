import * as React from "react";
import { Check, ChevronsUpDown, Loader2, X } from "lucide-react";
import { cn } from "@/lib/utils.ts";
import { Button } from "@/components/ui/button";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import {
  Command,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";

export function Combobox({
  options = [],
  value,
  onChange,
  multiple = false,
  showClear = false,
  placeholder = "Select...",
  searchPlaceholder = "Search...",
  onSearch,
  disabled = false,
  loading = false,
  className,
  contentClassName,
  buttonClassName,
}) {
  const [open, setOpen] = React.useState(false);
  const [search, setSearch] = React.useState("");
  const inlineInputRef = React.useRef(null);
  const inlinePointerDownRef = React.useRef(false);

  const selectedValues = React.useMemo(() => {
    return multiple ? (Array.isArray(value) ? value : []) : value ? [value] : [];
  }, [multiple, value]);

  const selectedLabel = React.useMemo(() => {
    if (selectedValues.length === 0) return placeholder;
    if (multiple) return `${selectedValues.length} selected`;
    const option = options.find((item) => item.value === selectedValues[0]);
    return option?.label || selectedValues[0];
  }, [selectedValues, options, multiple, placeholder]);

  React.useEffect(() => {
    if (onSearch) {
      onSearch(search);
    }
  }, [search, onSearch]);

  const toggleValue = (val) => {
    if (!multiple) {
      onChange?.(val);
      setOpen(false);
      return;
    }
    const next = selectedValues.includes(val)
      ? selectedValues.filter((item) => item !== val)
      : [...selectedValues, val];
    onChange?.(next);
  };

  const removeValue = (val, event) => {
    event.stopPropagation();
    if (multiple) {
      onChange?.(selectedValues.filter((item) => item !== val));
    }
  };

  const commitSearchValue = () => {
    const trimmed = search.trim();
    if (!multiple || !trimmed) return;
    if (!selectedValues.includes(trimmed)) {
      onChange?.([...selectedValues, trimmed]);
    }
    setSearch("");
  };

  const clearSelection = (event) => {
    event.stopPropagation();
    if (multiple) {
      onChange?.([]);
    } else {
      onChange?.("");
    }
  };

  const hasOptions = options.length > 0;

  const handleOpenChange = (nextOpen) => {
    if (multiple && !nextOpen) {
      const active = document.activeElement;
      if (active === inlineInputRef.current || inlinePointerDownRef.current) {
        inlinePointerDownRef.current = false;
        return;
      }
    }
    setOpen(nextOpen);
  };

  return (
    <Popover open={open} onOpenChange={handleOpenChange}>
      <PopoverTrigger asChild>
        <Button
          variant="outline"
          role="combobox"
          aria-expanded={open}
          disabled={disabled}
          className={cn(
            "w-full justify-between gap-2 text-sm font-normal h-auto min-h-9",
            selectedValues.length === 0 && "text-muted-foreground",
            buttonClassName
          )}
        >
          <span className="flex flex-1 flex-wrap items-center gap-1">
            {multiple ? (
              selectedValues.map((val) => (
                <span
                  key={val}
                  className="inline-flex items-center gap-1 rounded-md bg-secondary px-1.5 py-0.5 text-xs"
                >
                  <span className="max-w-[120px] truncate">{val}</span>
                  <span
                    role="button"
                    tabIndex={0}
                    className="rounded-sm hover:bg-muted-foreground/20"
                    onClick={(e) => removeValue(val, e)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") removeValue(val, e);
                    }}
                  >
                    <X className="h-3 w-3" />
                  </span>
                </span>
              ))
            ) : (
              <span className="truncate">{selectedLabel}</span>
            )}
            {multiple && (
              <input
                ref={inlineInputRef}
                className="min-w-[80px] flex-1 bg-transparent text-sm outline-none placeholder:text-muted-foreground"
                placeholder={selectedValues.length === 0 ? placeholder : ""}
                value={search}
                onChange={(event) => setSearch(event.target.value)}
                onPointerDown={() => {
                  inlinePointerDownRef.current = true;
                  setOpen(true);
                }}
                onFocus={() => setOpen(true)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    event.preventDefault();
                    commitSearchValue();
                  }
                }}
              />
            )}
          </span>
          <span className="flex shrink-0 items-center gap-1">
            {showClear && selectedValues.length > 0 && !multiple && (
              <span
                role="button"
                tabIndex={0}
                className="rounded-sm p-1 hover:bg-muted"
                onClick={clearSelection}
                onKeyDown={(event) => {
                  if (event.key === "Enter") clearSelection(event);
                }}
              >
                <X className="h-3 w-3" />
              </span>
            )}
            <ChevronsUpDown className="h-4 w-4 opacity-50" />
          </span>
        </Button>
      </PopoverTrigger>
      <PopoverContent className={cn("w-[300px] p-0", contentClassName)} align="start">
        <Command shouldFilter={false}>
          <CommandInput
            placeholder={searchPlaceholder}
            value={search}
            onValueChange={setSearch}
          />
          <CommandList>
            {loading ? (
              <div className="flex items-center justify-center gap-2 py-6 text-sm text-muted-foreground">
                <Loader2 className="h-4 w-4 animate-spin" />
                Loading...
              </div>
            ) : !hasOptions && search.length > 0 ? (
              <div className="py-6 text-center text-sm text-muted-foreground">
                No results found.
              </div>
            ) : !hasOptions ? (
              <div className="py-6 text-center text-sm text-muted-foreground">
                Type to search...
              </div>
            ) : (
              <CommandGroup>
                {options.map((option) => {
                  const selected = selectedValues.includes(option.value);
                  return (
                    <CommandItem
                      key={option.value}
                      value={option.value}
                      onSelect={() => toggleValue(option.value)}
                    >
                      <Check className={cn("mr-2 h-4 w-4", selected ? "opacity-100" : "opacity-0")} />
                      {option.label}
                    </CommandItem>
                  );
                })}
              </CommandGroup>
            )}
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );
}

// Legacy export for backwards compatibility - no longer needed with inline chips
export function ComboboxChips({ values = [], onRemove, className }) {
  if (!values.length) return null;
  return (
    <div className={cn("mt-2 flex flex-wrap gap-2", className)}>
      {values.map((value) => (
        <button
          key={value}
          type="button"
          className="inline-flex items-center gap-1 rounded-full border border-border bg-secondary px-2 py-1 text-xs"
          onClick={() => onRemove?.(value)}
        >
          <span className="max-w-[180px] truncate">{value}</span>
          <X className="h-3 w-3" />
        </button>
      ))}
    </div>
  );
}
