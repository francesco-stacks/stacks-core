export * from "./columnsConfig.ts";

export const DEFAULT_COLUMNS = COLUMN_DEFS.filter((c) => c.default)
  .map((c) => c.key)
  .filter((key) => !ALWAYS_HIDDEN_KEYS.has(key));

export const sanitizeSelectedColumns = (values) => {
  const base = Array.isArray(values)
    ? values.filter((key) => !ALWAYS_HIDDEN_KEYS.has(key))
    : [];
  ALWAYS_VISIBLE_KEYS.forEach((key) => {
    if (!base.includes(key)) base.push(key);
  });
  return base.length > 0 ? base : DEFAULT_COLUMNS;
};
