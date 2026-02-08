import * as React from "react"
import { PreviewCard as HoverCardPrimitive } from "@base-ui/react"

import { cn } from "@/lib/utils.ts"

const HoverCardDelayContext = React.createContext({});

function HoverCard({ openDelay, closeDelay, ...props }) {
  const delayValue = React.useMemo(
    () => ({ delay: openDelay, closeDelay }),
    [openDelay, closeDelay]
  );
  return (
    <HoverCardDelayContext.Provider value={delayValue}>
      <HoverCardPrimitive.Root {...props} />
    </HoverCardDelayContext.Provider>
  );
}

function HoverCardTrigger({ asChild, children, delay: delayProp, closeDelay: closeDelayProp, ...props }) {
  const ctx = React.useContext(HoverCardDelayContext);
  const delay = delayProp ?? ctx.delay;
  const closeDelay = closeDelayProp ?? ctx.closeDelay;
  const triggerProps = { ...props };
  if (delay != null) triggerProps.delay = delay;
  if (closeDelay != null) triggerProps.closeDelay = closeDelay;

  if (asChild) {
    return <HoverCardPrimitive.Trigger render={React.Children.only(children)} {...triggerProps} />;
  }
  return <HoverCardPrimitive.Trigger {...triggerProps}>{children}</HoverCardPrimitive.Trigger>;
}

function HoverCardContent({ className, align = "center", side, sideOffset = 4, style, ...props }) {
  return (
    <HoverCardPrimitive.Portal>
      <HoverCardPrimitive.Positioner alignment={align} side={side} sideOffset={sideOffset}>
        <HoverCardPrimitive.Popup
          className={cn(
            "z-50 w-64 rounded-[10px] border border-border p-4 text-popover-foreground shadow-md outline-none data-open:animate-in data-closed:animate-out data-closed:fade-out-0 data-open:fade-in-0 data-closed:zoom-out-95 data-open:zoom-in-95",
            className
          )}
          style={{ backgroundColor: 'var(--popover)', ...style }}
          {...props}
        />
      </HoverCardPrimitive.Positioner>
    </HoverCardPrimitive.Portal>
  )
}

export { HoverCard, HoverCardTrigger, HoverCardContent }
