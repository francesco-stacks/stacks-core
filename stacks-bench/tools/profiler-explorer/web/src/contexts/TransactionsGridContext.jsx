import React, { createContext, useContext, useRef } from "react";

/**
 * Context for transactions grid callbacks and configuration.
 * 
 * This context provides stable references to callbacks and config used by grid cell components.
 * By using refs internally, we ensure that:
 * 1. The context value remains stable (doesn't trigger re-renders of column definitions)
 * 2. Cell components always call the latest version of callbacks
 * 3. The columns array doesn't need to be recreated when callbacks change
 */

const TransactionsGridContext = createContext(null);

export function TransactionsGridProvider({ children, callbacks }) {
  // Store callbacks in a ref to avoid triggering re-renders
  const callbacksRef = useRef(callbacks);
  callbacksRef.current = callbacks;

  // Create stable wrapper functions that read from the ref
  const stableApi = useRef({
    // View trace callback
    onViewTrace: (row) => callbacksRef.current.onViewTrace?.(row),
    
    // Heat cell callbacks
    getHeatPercent: (colKey, value) => callbacksRef.current.getHeatPercent?.(colKey, value),
    get heatConfig() {
      return callbacksRef.current.heatConfig;
    },
    get heatMaxes() {
      return callbacksRef.current.heatMaxes;
    },
    get heatStyleByKey() {
      return callbacksRef.current.heatStyleByKey;
    },
    get heatColorByKey() {
      return callbacksRef.current.heatColorByKey;
    },
    get numberFormat() {
      return callbacksRef.current.numberFormat;
    },
    
    // Heat header configuration callbacks
    toggleHeat: (colKey) => callbacksRef.current.toggleHeat?.(colKey),
    setHeatMin: (colKey, val) => callbacksRef.current.setHeatMin?.(colKey, val),
    setHeatMax: (colKey, val) => callbacksRef.current.setHeatMax?.(colKey, val),
    setHeatStyleForKey: (colKey, style) => callbacksRef.current.setHeatStyleForKey?.(colKey, style),
    setHeatColorForKey: (colKey, color) => callbacksRef.current.setHeatColorForKey?.(colKey, color),
    get heatColorOptions() {
      return callbacksRef.current.heatColorOptions;
    },
    get minWallFilterMs() {
      return callbacksRef.current.minWallFilterMs;
    },
    setMinWallFilterMs: (val) => callbacksRef.current.setMinWallFilterMs?.(val),
  });

  return (
    <TransactionsGridContext.Provider value={stableApi.current}>
      {children}
    </TransactionsGridContext.Provider>
  );
}

export function useTransactionsGridContext() {
  const context = useContext(TransactionsGridContext);
  if (!context) {
    throw new Error("useTransactionsGridContext must be used within a TransactionsGridProvider");
  }
  return context;
}

export default TransactionsGridContext;
