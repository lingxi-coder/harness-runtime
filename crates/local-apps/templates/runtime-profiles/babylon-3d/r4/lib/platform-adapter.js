import {
  FALLBACK_DEVICE_CONTEXT,
  getDeviceContext,
  normalizeDeviceContext,
} from "./lingxi-bridge";

/// Host-managed. Derives the platform facts that something actually consumes.
///
/// The previous version of this file published `stateLayer: "ripple"`,
/// `navigation: "tabs"` and `supportsSidebar` — none of which had a single
/// consumer anywhere in the template. Ripples and platform chrome are now real
/// because `ionicMode` feeds `setupIonicReact({ mode })`, not because a table
/// says the word.

const DEFAULT_ADAPTER_KEY = "desktop:desktop";

/// `ios` renders Apple's design language, `md` renders Material. Ionic would
/// otherwise sniff the user agent, which is the wrong authority here: the host
/// already knows which client is embedding the WebView, and an Android tablet
/// running a desktop-class UA would guess wrong.
function ionicModeFor(os) {
  return os === "ios" ? "ios" : "md";
}

const PLATFORM_ADAPTERS = {
  "ios:iphone": { controlDensity: 44, fontFamily: "-apple-system, BlinkMacSystemFont, sans-serif" },
  "ios:tablet": { controlDensity: 44, fontFamily: "-apple-system, BlinkMacSystemFont, sans-serif" },
  "android:phone": { controlDensity: 48, fontFamily: "Roboto, sans-serif" },
  "android:tablet": { controlDensity: 48, fontFamily: "Roboto, sans-serif" },
  "desktop:desktop": { controlDensity: 40, fontFamily: "Inter, system-ui, sans-serif" },
};

export function getPlatformAdapter(context = getDeviceContext()) {
  const safeContext = normalizeDeviceContext(context);
  const key = `${safeContext.os}:${safeContext.formFactor}`;
  const resolved = key === "ios:ipad" ? "ios:tablet" : key;
  return {
    ...(PLATFORM_ADAPTERS[resolved] ?? PLATFORM_ADAPTERS[DEFAULT_ADAPTER_KEY]),
    key,
    ionicMode: ionicModeFor(safeContext.os),
    context: safeContext,
  };
}

/// The custom properties `styles/foundation.css` reads. Applied to
/// `document.documentElement` by the provider — NOT to a screen component.
///
/// They used to be spread onto the root element of `app/screens/home-screen.jsx`,
/// which the generator rewrites on every build; the first app that replaced that
/// screen silently lost its safe-area insets and its platform styling with it.
export function platformStyle(adapter) {
  const fallback = PLATFORM_ADAPTERS[DEFAULT_ADAPTER_KEY];
  const safe = adapter && typeof adapter === "object" ? adapter : fallback;
  const safeArea = safe.context?.safeArea ?? FALLBACK_DEVICE_CONTEXT.safeArea;
  return {
    "--safe-area-top": `${safeArea.top}px`,
    "--safe-area-right": `${safeArea.right}px`,
    "--safe-area-bottom": `${safeArea.bottom}px`,
    "--safe-area-left": `${safeArea.left}px`,
    "--platform-control-min": `${safe.controlDensity ?? fallback.controlDensity}px`,
    "--platform-font": safe.fontFamily ?? fallback.fontFamily,
  };
}
