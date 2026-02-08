import React, { createContext, useContext, useRef } from "react";

/**
 * Context for profiler grid callbacks and configuration.
 * 
 * This context provides stable references to callbacks and config used by grid cell components.
 * By using refs internally, we ensure that:
 * 1. The context value remains stable (doesn't trigger re-renders of column definitions)
 * 2. Cell components always call the latest version of callbacks
 * 3. The visibleColumns array doesn't need to be recreated when callbacks change
 */

const ProfilerGridContext = createContext(null);

export function ProfilerGridProvider({ children, callbacks }) {
  // Store callbacks in a ref to avoid triggering re-renders
  const callbacksRef = useRef(callbacks);
  callbacksRef.current = callbacks;

  // Create stable wrapper functions that read from the ref
  // These are called during render, so they need to return the current value
  const stableApi = useRef({
    // SpanCell callbacks
    toggleChain: (rowId) => callbacksRef.current.toggleChain?.(rowId),
    expandChainTo: (segments, segmentIndex) => callbacksRef.current.expandChainTo?.(segments, segmentIndex),
    focusNode: (rowId) => callbacksRef.current.focusNode?.(rowId),
    getSpanVizValue: (row) => callbacksRef.current.getSpanVizValue?.(row),
    get spanVizConfig() {
      return callbacksRef.current.spanVizConfig;
    },
    
    // HeatCell callbacks
    getHeatBounds: (colKey) => callbacksRef.current.getHeatBounds?.(colKey),
    getColumnValue: (colKey, row) => callbacksRef.current.getColumnValue?.(colKey, row),
    get heatColorByKey() {
      return callbacksRef.current.heatColorByKey;
    },
    get heatStyleByKey() {
      return callbacksRef.current.heatStyleByKey;
    },
    get numberFormat() {
      return callbacksRef.current.numberFormat;
    },
    
    // Heat header configuration callbacks
    get heatConfig() {
      return callbacksRef.current.heatConfig;
    },
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
    get defaultHeatStyle() {
      return callbacksRef.current.defaultHeatStyle;
    },
    get defaultHeatColor() {
      return callbacksRef.current.defaultHeatColor;
    },
    
    // Span header configuration callbacks
    setSpanVizConfig: (config) => callbacksRef.current.setSpanVizConfig?.(config),
  });

  return (
    <ProfilerGridContext.Provider value={stableApi.current}>
      {children}
    </ProfilerGridContext.Provider>
  );
}

export function useProfilerGridContext() {
  const context = useContext(ProfilerGridContext);
  if (!context) {
    throw new Error("useProfilerGridContext must be used within a ProfilerGridProvider");
  }
  return context;
}

export default ProfilerGridContext;
