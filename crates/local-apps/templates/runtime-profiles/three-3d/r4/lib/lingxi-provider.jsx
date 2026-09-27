import { createContext, useContext, useEffect, useMemo, useState } from "react";
import { getDeviceContext, getLingXiBridge } from "@/lib/lingxi-bridge";
import { getPlatformAdapter, platformStyle } from "@/lib/platform-adapter";

const LingXiBridgeContext = createContext(null);

function readSnapshot() {
  const bridge = getLingXiBridge();
  const device = getDeviceContext();
  const adapter = getPlatformAdapter(device);
  // `bridgeResolved` is NOT `bridgeReady`. It answers "has the question been
  // settled yet", which is what a consumer that must choose once — Ionic's
  // `mode`, say — has to wait for. `bridgeReady` stays false both while the
  // poll is still running and after it has given up, and those two states need
  // different behaviour: hold the first paint, versus render with the fallback.
  return {
    bridge,
    bridgeReady: bridge !== null,
    bridgeResolved: bridge !== null,
    device,
    adapter,
  };
}

/// The native `deviceContext` is LIVE — the host defines `viewport`,
/// `safeArea`, `colorScheme`, `reducedMotion` and `inputMode` as getters, so
/// reading it again always returns current values. But `getDeviceContext`
/// normalizes into a plain object, which snapshots those getters at call time.
/// Without this comparison-and-resubscribe the provider would freeze the very
/// first read: an iPad rotation, a Stage Manager / Split View resize, or a
/// dark-mode toggle would never reach the app.
function sameDevice(a, b) {
  return (
    a.os === b.os &&
    a.formFactor === b.formFactor &&
    a.colorScheme === b.colorScheme &&
    a.reducedMotion === b.reducedMotion &&
    a.inputMode === b.inputMode &&
    a.viewport.width === b.viewport.width &&
    a.viewport.height === b.viewport.height &&
    a.safeArea.top === b.safeArea.top &&
    a.safeArea.right === b.safeArea.right &&
    a.safeArea.bottom === b.safeArea.bottom &&
    a.safeArea.left === b.safeArea.left
  );
}

const MEDIA_QUERIES = [
  "(prefers-color-scheme: dark)",
  "(prefers-reduced-motion: reduce)",
  "(pointer: fine)",
];

export function LingXiBridgeProvider({ children }) {
  const [snapshot, setSnapshot] = useState(readSnapshot);

  useEffect(() => {
    if (snapshot.bridgeReady) return undefined;

    let frame = 0;
    let attempts = 0;
    const refresh = () => {
      const next = readSnapshot();
      if (next.bridgeReady || attempts >= 120) {
        // Giving up is itself an answer: mark it resolved so a consumer that is
        // holding its first paint renders with the fallback context instead of
        // waiting forever.
        setSnapshot({ ...next, bridgeResolved: true });
        return;
      }
      attempts += 1;
      frame = window.requestAnimationFrame(refresh);
    };
    frame = window.requestAnimationFrame(refresh);
    return () => window.cancelAnimationFrame(frame);
  }, [snapshot.bridgeReady]);

  // Resubscribe once the bridge exists. Every source that can change a
  // deviceContext getter is watched: window resize covers rotation and iPad
  // multitasking, visualViewport covers the software keyboard and pinch-zoom
  // insets, and the media queries cover appearance, motion and input-mode.
  useEffect(() => {
    if (!snapshot.bridgeReady) return undefined;

    const resync = () => {
      setSnapshot((current) => {
        const next = readSnapshot();
        return current.bridge === next.bridge && sameDevice(current.device, next.device)
          ? current
          : next;
      });
    };

    window.addEventListener("resize", resync);
    window.addEventListener("orientationchange", resync);
    window.addEventListener("pageshow", resync);
    document.addEventListener("visibilitychange", resync);
    const viewport = window.visualViewport ?? null;
    viewport?.addEventListener("resize", resync);
    viewport?.addEventListener("scroll", resync);

    const lists = MEDIA_QUERIES.map((query) => window.matchMedia?.(query)).filter(
      Boolean,
    );
    for (const list of lists) list.addEventListener("change", resync);

    // The first paint may land before the host has applied safe-area insets,
    // so take one catch-up read instead of trusting the mount-time snapshot.
    resync();

    return () => {
      window.removeEventListener("resize", resync);
      window.removeEventListener("orientationchange", resync);
      window.removeEventListener("pageshow", resync);
      document.removeEventListener("visibilitychange", resync);
      viewport?.removeEventListener("resize", resync);
      viewport?.removeEventListener("scroll", resync);
      for (const list of lists) list.removeEventListener("change", resync);
    };
  }, [snapshot.bridgeReady]);

  const style = useMemo(() => platformStyle(snapshot.adapter), [snapshot.adapter]);

  // Publish the platform facts on <html>, where no generated code can drop
  // them.
  //
  // These used to live on the root element of `app/screens/home-screen.jsx` —
  // the one file every prompt tells the generator to rewrite first. An app that
  // replaced that screen lost `data-platform`, `data-input-mode` and every
  // safe-area custom property along with it, and nothing failed: the styles
  // simply stopped applying. <html> is outside the agent-writable roots, so the
  // contract cannot be broken by generated source.
  useEffect(() => {
    const root = document.documentElement;
    const { adapter } = snapshot;

    root.dataset.platform = adapter.key;
    root.dataset.inputMode = adapter.context.inputMode;
    root.dataset.reducedMotion = String(adapter.context.reducedMotion === true);
    root.classList.toggle("ion-palette-dark", adapter.context.colorScheme === "dark");

    for (const [property, propertyValue] of Object.entries(style)) {
      root.style.setProperty(property, propertyValue);
    }
  }, [snapshot, style]);

  const value = useMemo(
    () => ({ ...snapshot, platformStyle: style }),
    [snapshot, style],
  );

  return (
    <LingXiBridgeContext.Provider value={value}>
      {children}
    </LingXiBridgeContext.Provider>
  );
}

export function useLingXi() {
  const value = useContext(LingXiBridgeContext);
  if (!value) throw new Error("useLingXi must be used inside LingXiBridgeProvider");
  return value;
}
