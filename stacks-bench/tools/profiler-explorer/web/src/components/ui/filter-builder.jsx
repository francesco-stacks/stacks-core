import React, {
  Fragment,
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";
import { Loader2, Plus, X } from "lucide-react";
import { Button } from "./button";
import { Input } from "./input";
import { Popover, PopoverTrigger, PopoverContent } from "./popover";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
} from "./select";

// ---------------------------------------------------------------------------
// Operator definitions
// ---------------------------------------------------------------------------

const TEXT_OPERATORS = [
  { id: "contains", label: "contains" },
  { id: "notContains", label: "does not contain" },
  { id: "equal", label: "equals" },
  { id: "notEqual", label: "does not equal" },
  { id: "beginsWith", label: "begins with" },
  { id: "endsWith", label: "ends with" },
];

const NUMBER_OPERATORS = [
  { id: "equal", label: "=" },
  { id: "notEqual", label: "≠" },
  { id: "greater", label: ">" },
  { id: "greaterOrEqual", label: "≥" },
  { id: "less", label: "<" },
  { id: "lessOrEqual", label: "≤" },
];

/** Operators that support the multi-select chip list (textual IN) */
const MULTI_VALUE_OPS = new Set(["equal", "contains", "beginsWith", "endsWith"]);

/** Operators for enum (checkbox-list) fields. */
const ENUM_OPERATORS = [
  { id: "is", label: "is" },
  { id: "isNot", label: "is not" },
];

function getOperators(field) {
  if (field?.operators) return field.operators;
  if (field?.type === "enum") return ENUM_OPERATORS;
  return field?.type === "number" ? NUMBER_OPERATORS : TEXT_OPERATORS;
}

function getOperatorLabel(field, opId) {
  return getOperators(field).find((o) => o.id === opId)?.label || opId;
}

function getFieldLabel(fields, fieldId) {
  return fields.find((f) => f.id === fieldId)?.label || fieldId;
}

/** Whether a field supports multi-value (chip) selection. */
function supportsMultiValue(field, operator) {
  if (field?.type === "enum") return true;
  return field?.type !== "number" && MULTI_VALUE_OPS.has(operator);
}

// ---------------------------------------------------------------------------
// Stable unique IDs
// ---------------------------------------------------------------------------

let _nextId = 1;
const uid = () => `_fr${_nextId++}`;

// ---------------------------------------------------------------------------
// AutocompleteInput – text input with debounced async suggestion list
// Re-mount via `key` prop when the field changes to clear stale results.
// ---------------------------------------------------------------------------

function AutocompleteInput({
  value,
  onChange,
  onCommit,
  fieldId,
  fieldType,
  options,
  placeholder,
}) {
  const [query, setQuery] = useState(value ?? "");
  const [results, setResults] = useState([]);
  const [loading, setLoading] = useState(false);
  const [open, setOpen] = useState(false);
  const [hlIndex, setHlIndex] = useState(-1);
  const abortRef = useRef(null);
  const inputRef = useRef(null);
  const listRef = useRef(null);
  // Flag: suppress the next autocomplete fetch (set after selecting an item)
  const suppressRef = useRef(false);

  // Sync external value
  useEffect(() => setQuery(value ?? ""), [value]);

  // Debounced fetch whenever the query changes (only after ≥1 char typed)
  useEffect(() => {
    if (!options || !fieldId || fieldType === "number") return;

    // Don't trigger autocomplete on empty input
    if (!query || query.length === 0) {
      setResults([]);
      setOpen(false);
      return;
    }

    // If just selected an item, skip this fetch cycle
    if (suppressRef.current) {
      suppressRef.current = false;
      return;
    }

    abortRef.current?.abort();
    const controller = new AbortController();
    abortRef.current = controller;

    const delay = 150;
    const timer = setTimeout(async () => {
      setLoading(true);
      try {
        const items = await options(fieldId, query ?? "", controller.signal);
        if (!controller.signal.aborted) {
          setResults(items || []);
          setOpen(true);
          setHlIndex(-1);
        }
      } catch {
        if (!controller.signal.aborted) setResults([]);
      } finally {
        if (!controller.signal.aborted) setLoading(false);
      }
    }, delay);

    return () => {
      clearTimeout(timer);
      controller.abort();
    };
  }, [fieldId, query, options, fieldType]);

  const handleInput = (e) => {
    const v = e.target.value;
    setQuery(v);
    onChange(v);
  };

  const select = (item) => {
    suppressRef.current = true;
    setQuery(item);
    onChange(item);
    setOpen(false);
    setResults([]);
    // Notify the parent that user committed a value (for multi-select chip add)
    onCommit?.(item);
    inputRef.current?.focus();
  };

  const handleKeyDown = (e) => {
    if (!open || results.length === 0) {
      if (e.key === "Escape") setOpen(false);
      // Enter without dropdown open = commit typed value
      if (e.key === "Enter" && query.trim()) {
        e.preventDefault();
        onCommit?.(query.trim());
      }
      return;
    }
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setHlIndex((i) => Math.min(i + 1, results.length - 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setHlIndex((i) => Math.max(i - 1, -1));
    } else if (e.key === "Enter") {
      e.preventDefault();
      if (hlIndex >= 0) {
        select(results[hlIndex]);
      } else if (query.trim()) {
        // Nothing highlighted — commit the raw typed text
        suppressRef.current = true;
        setOpen(false);
        setResults([]);
        onCommit?.(query.trim());
      }
    } else if (e.key === "Escape") {
      setOpen(false);
    }
  };

  // Keep highlighted item in view
  useEffect(() => {
    if (hlIndex >= 0 && listRef.current) {
      listRef.current.children[hlIndex]?.scrollIntoView({ block: "nearest" });
    }
  }, [hlIndex]);

  return (
    <div className="fb-autocomplete">
      <div className="fb-autocomplete-input-row">
        <Input
          ref={inputRef}
          type={fieldType === "number" ? "number" : "text"}
          value={query}
          onChange={handleInput}
          onKeyDown={handleKeyDown}
          onFocus={() => query && results.length && setOpen(true)}
          onBlur={() => setTimeout(() => setOpen(false), 200)}
          placeholder={placeholder || "Enter value…"}
          className="fb-value-input"
          autoFocus
        />
        {loading && (
          <Loader2 className="fb-autocomplete-spinner h-4 w-4 animate-spin" />
        )}
      </div>
      {open && results.length > 0 && (
        <div className="fb-autocomplete-list" ref={listRef}>
          {results.map((item, i) => (
            <div
              key={item}
              className={`fb-autocomplete-item${i === hlIndex ? " fb-hl" : ""}`}
              onMouseDown={(e) => {
                e.preventDefault();
                select(item);
              }}
              onMouseEnter={() => setHlIndex(i)}
            >
              {item}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// CheckboxList – checkbox-based value selector for enum fields
// ---------------------------------------------------------------------------

function CheckboxList({ enumValues, selected, onChange }) {
  const toggle = useCallback(
    (val) => {
      onChange((prev) =>
        prev.includes(val) ? prev.filter((v) => v !== val) : [...prev, val]
      );
    },
    [onChange]
  );

  if (!enumValues?.length) {
    return <div className="fb-enum-empty">No options available</div>;
  }

  return (
    <div className="fb-enum-list">
      {enumValues.map((item) => {
        const checked = selected.includes(item);
        return (
          <label key={item} className="fb-enum-option">
            <input
              type="checkbox"
              checked={checked}
              onChange={() => toggle(item)}
              className="fb-enum-checkbox"
            />
            <span className="fb-enum-label">{item}</span>
          </label>
        );
      })}
    </div>
  );
}

// ---------------------------------------------------------------------------
// FilterEditor – the popover body for adding / editing a single rule
// New layout:  Row 1: [Field selector]          [Condition type]
//              Row 2: [Value input]             [Modifier (opt)]
//              Row 3: (multi-select chips for textual IN)
// ---------------------------------------------------------------------------

/** Duration unit options for fields that have `modifier: "duration"`. */
const DURATION_UNITS = [
  { id: "ms", label: "ms" },
  { id: "us", label: "μs" },
  { id: "s",  label: "s"  },
];

function FilterEditor({ rule, fields, options, onApply, onCancel }) {
  const [field, setField] = useState(rule?.field || fields[0]?.id || "");
  const [operator, setOperator] = useState(rule?.operator || "");
  // If reopening a multi-value rule, start the input empty — chips already show the values
  const [value, setValue] = useState(rule?.values?.length ? "" : (rule?.value ?? ""));
  const [values, setValues] = useState(rule?.values ?? []);
  const [modifier, setModifier] = useState(rule?.modifier ?? "ms");
  // Key to force re-mount AutocompleteInput when field changes (clears stale results)
  const [acKey, setAcKey] = useState(0);
  // Track first mount so we don't wipe restored values on initial render
  const mountedRef = useRef(false);

  const selectedField = fields.find((f) => f.id === field);
  const operators = getOperators(selectedField);
  const isNumeric = selectedField?.type === "number";
  const isEnum = selectedField?.type === "enum";
  const hasDurationModifier = selectedField?.modifier === "duration";
  const isMulti = supportsMultiValue(selectedField, operator);

  // Reset operator + value + chips + autocomplete when field changes
  // (skip on initial mount so we don't wipe restored values from rule prop)
  useEffect(() => {
    if (!mountedRef.current) {
      mountedRef.current = true;
      return;
    }
    const ops = getOperators(fields.find((f) => f.id === field));
    if (ops.length && !ops.find((o) => o.id === operator)) {
      setOperator(ops[0].id);
    }
    setValue("");
    setValues([]);
    setAcKey((k) => k + 1);
  }, [field]); // eslint-disable-line react-hooks/exhaustive-deps

  // Set initial operator
  useEffect(() => {
    if (!operator && operators.length) setOperator(operators[0].id);
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // When operator changes and goes from multi→non-multi, collapse chips to single value
  useEffect(() => {
    if (!supportsMultiValue(selectedField, operator) && values.length > 0) {
      setValue(values[0] || "");
      setValues([]);
    }
  }, [operator]); // eslint-disable-line react-hooks/exhaustive-deps

  /** Add a value to the multi-select chip list. */
  const addChip = useCallback((v) => {
    const trimmed = String(v).trim();
    if (!trimmed) return;
    setValues((prev) => prev.includes(trimmed) ? prev : [...prev, trimmed]);
    setValue("");
    setAcKey((k) => k + 1);
  }, []);

  const removeChip = useCallback((v) => {
    setValues((prev) => prev.filter((x) => x !== v));
  }, []);

  const apply = () => {
    if (!field || !operator) return;
    // Enum fields require at least one checkbox selected
    if (isEnum && values.length === 0) return;
    const result = { field, operator };
    if (isMulti && values.length > 0) {
      result.values = values;
      result.value = values.join(", ");
    } else {
      result.value = value;
    }
    if (hasDurationModifier) {
      result.modifier = modifier;
    }
    onApply(result);
  };

  return (
    <div
      className="fb-editor"
      onKeyDown={(e) => {
        if (e.key === "Escape") onCancel();
      }}
    >
      {/* Row 1: Field selector + Operator selector */}
      <div className="fb-editor-row1">
        <Select value={field} onValueChange={setField}>
          <SelectTrigger className="fb-editor-field-select">
            <span className="truncate">
              {getFieldLabel(fields, field)}
            </span>
          </SelectTrigger>
          <SelectContent>
            {fields.map((f) => (
              <SelectItem key={f.id} value={f.id}>
                {f.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>

        <Select value={operator} onValueChange={setOperator}>
          <SelectTrigger className="fb-editor-op-select">
            <span className="truncate">
              {getOperatorLabel(selectedField, operator)}
            </span>
          </SelectTrigger>
          <SelectContent>
            {operators.map((o) => (
              <SelectItem key={o.id} value={o.id}>
                {o.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>

      {/* Row 2: Value input + optional modifier (or checkbox list for enum) */}
      {isEnum ? (
        <CheckboxList
          enumValues={selectedField?.enumValues}
          selected={values}
          onChange={setValues}
        />
      ) : (
      <div className="fb-editor-row2">
        {isNumeric ? (
          <Input
            type="number"
            value={value}
            onChange={(e) => setValue(e.target.value)}
            onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); apply(); } }}
            placeholder="Enter value…"
            className="fb-value-input-numeric"
            autoFocus
          />
        ) : (
          <AutocompleteInput
            key={acKey}
            value={value}
            onChange={setValue}
            onCommit={isMulti ? addChip : undefined}
            fieldId={field}
            fieldType={selectedField?.type}
            options={options}
            placeholder={isMulti && values.length ? "Add another…" : "Enter value…"}
          />
        )}

        {hasDurationModifier && (
          <Select value={modifier} onValueChange={setModifier}>
            <SelectTrigger className="fb-editor-modifier-select">
              <span className="truncate">
                {DURATION_UNITS.find((u) => u.id === modifier)?.label || modifier}
              </span>
            </SelectTrigger>
            <SelectContent>
              {DURATION_UNITS.map((u) => (
                <SelectItem key={u.id} value={u.id}>
                  {u.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        )}
      </div>
      )}

      {/* Row 3: Multi-select chips (text fields only, not enum) */}
      {!isEnum && isMulti && values.length > 0 && (
        <div className="fb-editor-chips">
          {values.map((v) => (
            <span key={v} className="fb-selected-chip">
              <span className="fb-selected-chip-label" title={v}>{v}</span>
              <button
                type="button"
                className="fb-selected-chip-x"
                onClick={() => removeChip(v)}
                aria-label={`Remove ${v}`}
              >
                <X size={12} />
              </button>
            </span>
          ))}
        </div>
      )}

      {/* Actions */}
      <div className="fb-editor-actions">
        <Button variant="ghost" size="sm" onClick={onCancel}>
          Cancel
        </Button>
        <Button size="sm" onClick={apply}>
          Apply
        </Button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// GlueToggle – clickable and / or pill
// ---------------------------------------------------------------------------

function GlueToggle({ glue, onClick }) {
  return (
    <button
      type="button"
      className={`fb-glue${glue === "or" ? " fb-glue-or" : ""}`}
      onClick={onClick}
    >
      {glue}
    </button>
  );
}

// ---------------------------------------------------------------------------
// FilterChip – displays one rule with an X button
// ---------------------------------------------------------------------------

function FilterChip({ rule, fields, onEdit, onDelete }) {
  const field = fields.find((f) => f.id === rule.field);
  const opLabel = getOperatorLabel(field, rule.operator);
  const displayValue = rule.values?.length
    ? `${rule.values.length} value${rule.values.length > 1 ? "s" : ""}`
    : String(rule.value);
  const displaySuffix = rule.modifier && rule.modifier !== "ms"
    ? ` (${rule.modifier})`
    : "";
  return (
    <div className="fb-chip" role="button" tabIndex={0} onClick={onEdit}>
      <span className="fb-chip-field">{field?.label || rule.field}</span>
      <span className="fb-chip-op">{opLabel}</span>
      <span className="fb-chip-value">{displayValue}{displaySuffix}</span>
      <button
        type="button"
        className="fb-chip-x"
        tabIndex={-1}
        onClick={(e) => {
          e.stopPropagation();
          onDelete();
        }}
        aria-label="Remove filter"
      >
        <X size={14} />
      </button>
    </div>
  );
}

// ---------------------------------------------------------------------------
// GroupChip – displays a nested group as a bordered inline container
// ---------------------------------------------------------------------------

function GroupChip({ group, fields, options, onUpdate, onDelete }) {
  return (
    <div className="fb-group">
      <div className="fb-group-header">
        <button
          type="button"
          className="fb-group-x"
          onClick={onDelete}
          aria-label="Remove group"
        >
          <X size={12} />
        </button>
      </div>
      <FilterGroup
        value={group}
        fields={fields}
        onChange={onUpdate}
        options={options}
        depth={1}
      />
    </div>
  );
}

// ---------------------------------------------------------------------------
// FilterGroup – renders one level of rules + glue toggles (recursive)
// ---------------------------------------------------------------------------

function FilterGroup({ value, fields, onChange, options, depth = 0 }) {
  const [editingId, setEditingId] = useState(null);

  const rules = value?.rules || [];
  const glue = value?.glue || "and";

  const addRule = useCallback(
    (data) => {
      const rule = { id: uid(), ...data };
      onChange({ glue, rules: [...rules, rule] });
      setEditingId(null);
    },
    [glue, rules, onChange],
  );

  const updateRule = useCallback(
    (ruleId, data) => {
      onChange({
        ...value,
        rules: rules.map((r) => (r.id === ruleId ? { ...r, ...data } : r)),
      });
      setEditingId(null);
    },
    [value, rules, onChange],
  );

  const deleteRule = useCallback(
    (ruleId) => {
      onChange({ ...value, rules: rules.filter((r) => r.id !== ruleId) });
      if (editingId === ruleId) setEditingId(null);
    },
    [value, rules, onChange, editingId],
  );

  const toggleGlue = useCallback(() => {
    onChange({ ...value, glue: glue === "and" ? "or" : "and" });
  }, [value, glue, onChange]);

  const addGroup = useCallback(() => {
    const group = { id: uid(), glue: "or", rules: [] };
    onChange({ glue, rules: [...rules, group] });
  }, [glue, rules, onChange]);

  const updateGroup = useCallback(
    (groupId, newGroup) => {
      onChange({
        ...value,
        rules: rules.map((r) => (r.id === groupId ? { ...r, ...newGroup } : r)),
      });
    },
    [value, rules, onChange],
  );

  const handleApply = useCallback(
    (data) => {
      if (editingId && editingId !== "__new__") {
        updateRule(editingId, data);
      } else {
        addRule(data);
      }
    },
    [editingId, addRule, updateRule],
  );

  const closeEditor = useCallback(() => setEditingId(null), []);

  return (
    <div className="fb-bar">
      {rules.map((rule, index) => {
        const isGroup = Array.isArray(rule.rules);
        return (
          <Fragment key={rule.id}>
            {index > 0 && <GlueToggle glue={glue} onClick={toggleGlue} />}

            {isGroup ? (
              <GroupChip
                group={rule}
                fields={fields}
                options={options}
                onUpdate={(updated) => updateGroup(rule.id, updated)}
                onDelete={() => deleteRule(rule.id)}
              />
            ) : (
              <Popover
                open={editingId === rule.id}
                onOpenChange={(open) => {
                  if (!open) setEditingId(null);
                }}
              >
                <PopoverTrigger asChild>
                  <span>
                    <FilterChip
                      rule={rule}
                      fields={fields}
                      onEdit={() => setEditingId(rule.id)}
                      onDelete={() => deleteRule(rule.id)}
                    />
                  </span>
                </PopoverTrigger>
                <PopoverContent
                  side="bottom"
                  align="start"
                  sideOffset={6}
                  className="fb-popover"
                >
                  <FilterEditor
                    rule={rule}
                    fields={fields}
                    options={options}
                    onApply={handleApply}
                    onCancel={closeEditor}
                  />
                </PopoverContent>
              </Popover>
            )}
          </Fragment>
        );
      })}

      {/* Add filter button */}
      <Popover
        open={editingId === "__new__"}
        onOpenChange={(open) => {
          if (open) setEditingId("__new__");
          else setEditingId(null);
        }}
      >
        <PopoverTrigger asChild>
          <Button variant="default" size="sm" className="fb-add-btn">
            Add filter
          </Button>
        </PopoverTrigger>
        <PopoverContent
          side="bottom"
          align="start"
          sideOffset={6}
          className="fb-popover"
        >
          <FilterEditor
            rule={null}
            fields={fields}
            options={options}
            onApply={handleApply}
            onCancel={closeEditor}
          />
        </PopoverContent>
      </Popover>

      {/* Add Group button – only at top level */}
      {depth === 0 && (
        <Button
          variant="outline"
          size="sm"
          className="fb-add-btn"
          onClick={addGroup}
        >
          <Plus size={14} className="mr-1" />
          Group
        </Button>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// FilterBuilder – the main exported component
// ---------------------------------------------------------------------------

/**
 * A fully custom, reusable filter-bar component with nested group support.
 *
 * @param {Object}   props
 * @param {Array}    props.fields   – field definitions `{ id, label, type, operators? }`
 * @param {Object}   props.value    – filter state `{ glue, rules: [rule|group] }`
 *        rule = `{ id, field, operator, value }`
 *        group = `{ id, glue, rules: [rule|group] }`
 * @param {Function} props.onChange – called with the updated filter-state object
 * @param {Function} [props.options] – async autocomplete `(fieldId, query, signal) => Promise<string[]>`
 */
export function FilterBuilder({ fields, value, onChange, options }) {
  return (
    <FilterGroup
      value={value}
      fields={fields}
      onChange={onChange}
      options={options}
      depth={0}
    />
  );
}
