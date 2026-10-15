// App runtime, inlined ahead of the fragment of an app saved from a
// visualization.
//
// The fragment was written against the `lingxi` global of the content
// runtime. An app has no conversation and no host channel, so this file
// implements the same global on the app's own origin: state lives in
// localStorage, seeded once from the visualization state the app library
// injects as `window.__lingxiAppBoot`; the theme follows the system; the calls
// that need a conversation report that they are unavailable.
(() => {
  "use strict";

  const STATE_KEY = "lingxi.state";
  const MAX_STATE_BYTES = 16 * 1024;
  const encoder = new TextEncoder();
  const darkScheme = window.matchMedia("(prefers-color-scheme: dark)");
  const handlers = { suspend: new Set(), theme: new Set(), state: new Set(), expanded: new Set() };

  function normalizeState(value) {
    const state = value && typeof value === "object" ? value : {};
    return {
      version: Number.isSafeInteger(state.version) ? state.version : 0,
      modelContent: state.modelContent ?? null,
      privateContent: state.privateContent ?? null,
    };
  }

  const clone = (value) => (value == null ? null : JSON.parse(JSON.stringify(value)));

  function subscribe(set, handler) {
    if (typeof handler !== "function") {
      throw new TypeError("handler must be a function");
    }
    set.add(handler);
    return () => set.delete(handler);
  }

  function notify(set, value) {
    for (const handler of set) {
      try {
        handler(value);
      } catch (error) {
        console.error(error);
      }
    }
  }

  // The saved state, or the seed on the first launch (and after the user
  // clears the app's data).
  function load() {
    try {
      const saved = localStorage.getItem(STATE_KEY);
      if (saved != null) {
        return normalizeState(JSON.parse(saved));
      }
    } catch (error) {
      console.error(error);
    }
    const seed = normalizeState(window.__lingxiAppBoot);
    try {
      localStorage.setItem(STATE_KEY, JSON.stringify(seed));
    } catch (error) {
      console.error(error);
    }
    return seed;
  }

  let confirmed = load();

  function snapshot() {
    return Object.freeze({
      version: confirmed.version,
      modelContent: clone(confirmed.modelContent),
      privateContent: clone(confirmed.privateContent),
    });
  }

  function saveState(next = {}) {
    const modelContent = "modelContent" in next ? next.modelContent : confirmed.modelContent;
    const privateContent = "privateContent" in next ? next.privateContent : confirmed.privateContent;
    let json;
    try {
      json = JSON.stringify({ modelContent: modelContent ?? null, privateContent: privateContent ?? null });
    } catch {
      return Promise.reject(new TypeError("state must be JSON-serializable"));
    }
    if (encoder.encode(json).length > MAX_STATE_BYTES) {
      return Promise.reject(new RangeError("state exceeds 16 KiB"));
    }
    const state = { version: confirmed.version + 1, ...JSON.parse(json) };
    try {
      localStorage.setItem(STATE_KEY, JSON.stringify(state));
    } catch (error) {
      return Promise.reject(new Error(`state not saved: ${error?.message ?? error}`));
    }
    confirmed = state;
    queueMicrotask(() => notify(handlers.state, snapshot()));
    return Promise.resolve({ version: state.version });
  }

  const currentMode = () => (darkScheme.matches ? "dark" : "light");
  let mode = currentMode();
  document.documentElement.dataset.theme = mode;
  darkScheme.addEventListener("change", () => {
    mode = currentMode();
    document.documentElement.dataset.theme = mode;
    notify(handlers.theme, { mode, tokens: {} });
  });

  const locale =
    typeof navigator.language === "string" && navigator.language !== "" ? navigator.language : "en";
  if (!document.documentElement.lang) {
    document.documentElement.lang = locale;
  }

  // Leaving the app (switching away, closing it) is the app's suspend: the
  // page may never run again, so handlers save now.
  let suspended = false;
  function suspend() {
    if (suspended) {
      return;
    }
    suspended = true;
    notify(handlers.suspend, undefined);
  }
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden") {
      suspend();
    } else {
      suspended = false;
    }
  });
  window.addEventListener("pagehide", suspend);
  window.addEventListener("pageshow", () => {
    suspended = false;
  });

  const api = Object.freeze({
    get state() {
      return snapshot();
    },
    get theme() {
      return { mode, tokens: {} };
    },
    get locale() {
      return locale;
    },
    // An app is always full screen.
    get expanded() {
      return true;
    },
    saveState,
    draftFollowup() {
      throw new Error("draftFollowup needs a conversation; this app runs on its own");
    },
    requestExpand() {},
    onSuspend: (handler) => subscribe(handlers.suspend, handler),
    onTheme: (handler) => subscribe(handlers.theme, handler),
    onState: (handler) => subscribe(handlers.state, handler),
    onExpanded: (handler) => subscribe(handlers.expanded, handler),
  });
  Object.defineProperty(window, "lingxi", { value: api, enumerable: true });
})();
