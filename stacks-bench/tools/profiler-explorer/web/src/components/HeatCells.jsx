import React from "react";

function formatNumberParts(value, { group, decimal, decimals }) {
  const sign = value < 0 ? "-" : "";
  const abs = Math.abs(value);
  const fixed = Number.isFinite(decimals) ? abs.toFixed(decimals) : String(abs);
  const [intPart, fracPart] = fixed.split(".");
  const grouped = group
    ? intPart.replace(/\B(?=(\d{3})+(?!\d))/g, group)
    : intPart;
  return {
    sign,
    int: grouped,
    frac: fracPart || "",
    decimal,
  };
}

function FormattedNumber({ value, format, decimals, className }) {
  if (value === null || value === undefined) {
    return <span className={`${className} numeric-zero`}>-</span>;
  }
  if (typeof value !== "number" || !Number.isFinite(value)) {
    if (typeof value === "string" && value.trim() === "0") {
      return <span className={`${className} numeric-zero`}>-</span>;
    }
    return <span className={className}>{String(value)}</span>;
  }
  if (value === 0) {
    return <span className={`${className} numeric-zero`}>-</span>;
  }
  const parts = formatNumberParts(value, {
    group: format.group,
    decimal: format.decimal,
    decimals,
  });
  return (
    <span className={className}>
      {parts.sign}
      {parts.int}
      {parts.frac ? (
        <span className="numeric-decimals">
          {parts.decimal}
          {parts.frac}
        </span>
      ) : null}
    </span>
  );
}

function isCompressed(row) {
  return Number.isFinite(row.chain_count) && row.chain_count > 0;
}

function isZeroDisplay(value) {
  if (value === null || value === undefined) return false;
  if (typeof value === "number") return value === 0;
  if (typeof value === "string") {
    const trimmed = value.trim();
    if (trimmed === "0") return true;
  }
  return false;
}

function hasDisplayValue(value) {
  if (value === null || value === undefined) return false;
  if (typeof value === "number" && value === 0) return false;
  if (typeof value === "string") {
    const trimmed = value.trim();
    if (trimmed === "-") return false;
    if (trimmed === "0") return false;
    if (trimmed.includes("/")) {
      const [left, right] = trimmed.split("/").map((part) => Number(part));
      if (Number.isFinite(left) && Number.isFinite(right)) {
        return !(left === 0 && right === 0);
      }
    }
  }
  return true;
}

function formatSideValue(value, format, decimals) {
  if (!Number.isFinite(value)) return String(value);
  if (value === 0) return "-";
  const parts = formatNumberParts(value, {
    group: format.group,
    decimal: format.decimal,
    decimals,
  });
  return `${parts.sign}${parts.int}${parts.frac ? `${parts.decimal}${parts.frac}` : ""}`;
}

export const HeatCell = React.memo(function HeatCell({ row, value, percent, format, numberFormat, heatStyle, heatColor }) {
  const pct = Number.isFinite(percent) ? Math.max(0, Math.min(100, percent)) : 0;
  const level = pct > 0 ? pct / 100 : 0;
  const alpha = level > 0 ? 0.04 + level * 0.2 : 0;
  const aggregated = isCompressed(row);
  const showAggregate = aggregated && hasDisplayValue(value);
  const dimmed = isZeroDisplay(value);
  return (
    <div
      className={`heat-cell heat-style-${heatStyle || "fill"} heat-color-${
        heatColor || "red"
      }`}
      style={{
        "--heat-alpha": alpha,
        "--heat-level": level,
        "--heat-width": `${pct}%`,
      }}
    >
      <span className="heat-fill" />
      <span className="heat-edge" />
      <span className="heat-meter" />
      <div className={`heat-text${dimmed ? " numeric-zero" : ""}`}>
        {showAggregate ? (
          <span className="aggregate-badge" title="Aggregated via chain compression">
            Σ
          </span>
        ) : null}
        <FormattedNumber
          value={value}
          format={numberFormat}
          decimals={format.decimals}
          className="numeric-value"
        />
      </div>
    </div>
  );
});

export function NumericCell({ row, value, format, numberFormat }) {
  const aggregated = isCompressed(row);
  const showAggregate = aggregated && hasDisplayValue(value);
  const isPair = typeof value === "string" && value.includes("/") && value.trim() !== "-";
  const dimmed = !isPair && isZeroDisplay(value);
  return (
    <div className={`numeric-cell${dimmed ? " numeric-zero" : ""}`}>
      {showAggregate ? (
        <span className="aggregate-badge" title="Aggregated via chain compression">
          Σ
        </span>
      ) : null}
      {isPair ? (
        <span className="numeric-text">
          {(() => {
            const [leftRaw, rightRaw] = value.split("/");
            const left = Number(leftRaw);
            const right = Number(rightRaw);
            const decimals = format?.decimals ?? 0;
            const leftFormatted = Number.isFinite(left)
              ? formatSideValue(left, numberFormat, decimals)
              : leftRaw;
            const rightFormatted = Number.isFinite(right)
              ? formatSideValue(right, numberFormat, decimals)
              : rightRaw;
            const leftZero = Number.isFinite(left) && left === 0;
            const rightZero = Number.isFinite(right) && right === 0;
            return (
              <>
                <span className={leftZero ? "numeric-zero-part" : undefined}>{leftFormatted}</span>
                <span className="numeric-separator">/</span>
                <span className={rightZero ? "numeric-zero-part" : undefined}>{rightFormatted}</span>
              </>
            );
          })()}
        </span>
      ) : (
        <FormattedNumber
          value={value}
          format={numberFormat}
          decimals={format?.decimals ?? 0}
          className="numeric-text"
        />
      )}
    </div>
  );
}
