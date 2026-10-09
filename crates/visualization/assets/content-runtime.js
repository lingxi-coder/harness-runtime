// LingXi content runtime, inlined ahead of the author fragment.
//
// The document runs in an opaque-origin sandbox. Its only channel out is the
// MessagePort the parent shell transfers once; this file exposes that channel
// as the `lingxi` global and reports the document height for auto-sizing.
(() => {
  "use strict";

  const boot = window.__lingxiVisualizationBoot || {};
  const generation = boot.generation;
  const MAX_STATE_BYTES = 16 * 1024;
  const MAX_DRAFT_CHARS = 2000;
  const SAVE_DEBOUNCE_MS = 250;
  const SUSPEND_HANDLER_MS = 1000;
  const TOKEN_NAME = /^--[a-z0-9-]{1,64}$/;
  const TOKEN_VALUE = /^[A-Za-z0-9 #%(),./-]{1,64}$/;
  const encoder = new TextEncoder();

  let port = null;
  const outbox = [];
  let confirmed = normalizeState(boot.state);
  let theme = boot.theme || { mode: "light", tokens: {} };
  let expanded = boot.expanded === true;
  let queued = null;
  let saveTimer = 0;
  let inflight = null;
  let nextRequestId = 1;
  let suspending = false;
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

  function post(type, payload = {}) {
    const message = { ...payload, type, generation };
    if (port) {
      port.postMessage(message);
    } else {
      outbox.push(message);
    }
  }

  function notify(set, value) {
    for (const handler of set) {
      try {
        handler(value);
      } catch (error) {
        reportError(error);
      }
    }
  }

  function subscribe(set, handler) {
    if (typeof handler !== "function") {
      throw new TypeError("handler must be a function");
    }
    set.add(handler);
    return () => set.delete(handler);
  }

  function snapshot() {
    return Object.freeze({
      version: confirmed.version,
      modelContent: clone(confirmed.modelContent),
      privateContent: clone(confirmed.privateContent),
    });
  }

  function applyTheme(next) {
    if (!next || typeof next !== "object") {
      return;
    }
    const mode = next.mode === "dark" ? "dark" : "light";
    let css = ":root{";
    for (const [name, value] of Object.entries(next.tokens || {})) {
      if (TOKEN_NAME.test(name) && typeof value === "string" && TOKEN_VALUE.test(value)) {
        css += `${name}:${value};`;
      }
    }
    css += "}";
    document.documentElement.dataset.theme = mode;
    const style = document.getElementById("lingxi-theme");
    if (style) {
      style.textContent = css;
    }
    theme = { mode, tokens: { ...(next.tokens || {}) } };
    notify(handlers.theme, theme);
  }

  function flushSave() {
    saveTimer = 0;
    if (inflight || !queued) {
      return;
    }
    const job = queued;
    queued = null;
    job.requestId = nextRequestId++;
    inflight = job;
    post("state.save", {
      requestId: job.requestId,
      baseVersion: confirmed.version,
      modelContent: JSON.stringify(job.modelContent ?? null),
      privateContent: JSON.stringify(job.privateContent ?? null),
    });
  }

  function settleSave(requestId, outcome) {
    if (!inflight || inflight.requestId !== requestId) {
      return;
    }
    const job = inflight;
    inflight = null;
    if (outcome.ok) {
      confirmed = { version: outcome.version, modelContent: job.modelContent, privateContent: job.privateContent };
      for (const waiter of job.waiters) {
        waiter.resolve({ version: outcome.version });
      }
      notify(handlers.state, snapshot());
    } else {
      if (outcome.state) {
        // A newer state won; adopt it so the next save rebases on it.
        confirmed = normalizeState(outcome.state);
        notify(handlers.state, snapshot());
      }
      for (const waiter of job.waiters) {
        waiter.reject(new Error(`state not saved: ${outcome.reason}`));
      }
    }
    if (queued && !saveTimer) {
      saveTimer = setTimeout(flushSave, 0);
    }
  }

  function saveState(next = {}) {
    if (suspending) {
      return Promise.reject(new Error("visualization is suspending"));
    }
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
    return new Promise((resolve, reject) => {
      queued ??= { waiters: [] };
      queued.modelContent = clone(modelContent);
      queued.privateContent = clone(privateContent);
      queued.waiters.push({ resolve, reject });
      clearTimeout(saveTimer);
      saveTimer = setTimeout(flushSave, SAVE_DEBOUNCE_MS);
    });
  }

  function draftFollowup(text) {
    if (typeof text !== "string" || text.trim().length === 0) {
      throw new TypeError("draft must be non-empty text");
    }
    if ([...text].length > MAX_DRAFT_CHARS) {
      throw new RangeError(`draft exceeds ${MAX_DRAFT_CHARS} characters`);
    }
    post("followup.draft", { text });
  }

  const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

  async function suspend() {
    suspending = true;
    try {
      await Promise.race([
        Promise.allSettled([...handlers.suspend].map((handler) => Promise.resolve().then(handler))),
        delay(SUSPEND_HANDLER_MS),
      ]);
    } catch (error) {
      reportError(error);
    }
    if (saveTimer) {
      clearTimeout(saveTimer);
      flushSave();
    }
    const deadline = Date.now() + SUSPEND_HANDLER_MS;
    while (inflight && Date.now() < deadline) {
      await delay(25);
    }
    post("suspended");
  }

  function onShellMessage(event) {
    const data = event.data;
    if (!data || typeof data !== "object" || data.generation !== generation) {
      return;
    }
    switch (data.type) {
      case "theme":
        applyTheme(data.theme);
        break;
      case "state.saved":
        settleSave(data.requestId, { ok: true, version: data.version });
        break;
      case "state.rejected":
        settleSave(data.requestId, { ok: false, reason: data.reason, state: data.state });
        break;
      case "suspend":
        void suspend();
        break;
      case "expanded":
        expanded = data.expanded === true;
        notify(handlers.expanded, expanded);
        scheduleResize();
        break;
      default:
        break;
    }
  }

  window.addEventListener("message", (event) => {
    const data = event.data;
    if (port || event.source !== window.parent || !data || data.type !== "lingxi.connect") {
      return;
    }
    if (data.generation !== generation || !event.ports || event.ports.length !== 1) {
      return;
    }
    port = event.ports[0];
    port.onmessage = onShellMessage;
    for (const message of outbox.splice(0)) {
      port.postMessage(message);
    }
    post("ready");
    scheduleResize();
  });

  let lastHeight = -1;
  let resizeFrame = 0;
  function scheduleResize() {
    if (resizeFrame) {
      return;
    }
    resizeFrame = requestAnimationFrame(() => {
      resizeFrame = 0;
      const root = document.documentElement;
      const height = Math.ceil(Math.max(root.scrollHeight, document.body ? document.body.scrollHeight : 0));
      if (Math.abs(height - lastHeight) >= 1) {
        lastHeight = height;
        post("resize", { height });
      }
    });
  }
  const observer = new ResizeObserver(scheduleResize);
  observer.observe(document.documentElement);
  document.addEventListener("DOMContentLoaded", () => {
    if (document.body) {
      observer.observe(document.body);
    }
    scheduleResize();
  });
  window.addEventListener("load", scheduleResize);

  function reportError(error) {
    post("error", { message: String(error?.message ?? error).slice(0, 500) });
  }
  window.addEventListener("error", (event) => reportError(event.error ?? event.message));
  window.addEventListener("unhandledrejection", (event) => reportError(event.reason));

  const api = Object.freeze({
    get state() {
      return snapshot();
    },
    get theme() {
      return { mode: theme.mode, tokens: { ...theme.tokens } };
    },
    get locale() {
      return typeof boot.locale === "string" ? boot.locale : "en";
    },
    get expanded() {
      return expanded;
    },
    saveState,
    draftFollowup,
    requestExpand: () => post("expand.request"),
    onSuspend: (handler) => subscribe(handlers.suspend, handler),
    onTheme: (handler) => subscribe(handlers.theme, handler),
    onState: (handler) => subscribe(handlers.state, handler),
    onExpanded: (handler) => subscribe(handlers.expanded, handler),
  });
  Object.defineProperty(window, "lingxi", { value: api, enumerable: true });
})();
