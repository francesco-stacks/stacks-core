import * as React from "react";

/**
 * Minimal Slot implementation (replaces @radix-ui/react-slot).
 * Merges its props into the single React element child,
 * allowing polymorphic rendering via the `asChild` pattern.
 */
const Slot = React.forwardRef(({ children, ...slotProps }, forwardedRef) => {
  const child = React.Children.only(children);
  if (!React.isValidElement(child)) return null;

  // Merge refs
  const childRef = child.ref;
  const ref = mergeRefs(forwardedRef, childRef);

  // Merge classNames
  const mergedClassName = [slotProps.className, child.props.className]
    .filter(Boolean)
    .join(" ") || undefined;

  // Merge styles
  const mergedStyle =
    slotProps.style || child.props.style
      ? { ...slotProps.style, ...child.props.style }
      : undefined;

  return React.cloneElement(child, {
    ...slotProps,
    ...child.props,
    ref,
    ...(mergedClassName !== undefined && { className: mergedClassName }),
    ...(mergedStyle !== undefined && { style: mergedStyle }),
  });
});
Slot.displayName = "Slot";

function mergeRefs(...refs) {
  return (value) => {
    for (const ref of refs) {
      if (typeof ref === "function") {
        ref(value);
      } else if (ref != null) {
        ref.current = value;
      }
    }
  };
}

export { Slot };
