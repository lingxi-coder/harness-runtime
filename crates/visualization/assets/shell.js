// Trusted shell for one inline visualization mount.
//
// Runs as the main frame of a dedicated native WebView. It owns the only
// channel to the native host and talks to the sandboxed content frame over a
// MessagePort handed over once per mount. It never listens for window-level
// messages: after the port changes hands, nothing else can address it.
(() => {
  "use strict";

  const MAX_STATE_BYTES = 16 * 1024;
  const MAX_DRAFT_CHARS = 2000;
  const DRAFT_INTERVAL_MS = 2000;
  const SAVE_INTERVAL_MS = 150;
  const RESIZE_INTERVAL_MS = 50;
  const ERROR_INTERVAL_MS = 1000;
  const MAX_ERROR_CHARS = 500;
  const SUSPEND_TIMEOUT_MS = 1500;
  const MIN_HEIGHT = 32;
  const DEFAULT_MAX_HEIGHT = 720;

  const frame = document.getElementById("content");
  const docPrefix = new URL("/doc/", window.location.href).href;
  const encoder = new TextEncoder();
  let mount = null;

  const parse = (raw) => {
    try {
      return typeof raw === "string" ? JSON.parse(raw) : raw;
    } catch {
      return null;
    }
  };

  // Each host injects one bridge object into this main frame only:
  // Electron's preload and Android's WebMessageListener expose
  // `window.lingxiVisualization`; iOS exposes a WKScriptMessageHandler and
  // delivers replies through `__lingxiVisualizationDeliver`.
  const send = (() => {
    const bridge = window.lingxiVisualization;
    if (bridge && typeof bridge.postMessage === "function") {
      if (typeof bridge.onHostMessage === "function") {
        bridge.onHostMessage((raw) => receive(parse(raw)));
      } else {
        bridge.onmessage = (event) => receive(parse(event.data));
      }
      return (message) => bridge.postMessage(JSON.stringify(message));
    }
    const webkit = window.webkit?.messageHandlers?.lingxiVisualization;
    if (webkit) {
      Object.defineProperty(window, "__lingxiVisualizationDeliver", {
        value: (raw) => receive(parse(raw)),
        writable: false,
        configurable: false,
      });
      return (message) => webkit.postMessage(JSON.stringify(message));
    }
    return () => {};
  })();

  const isGeneration = (value) => Number.isSafeInteger(value) && value > 0;
  const isCount = (value) => Number.isSafeInteger(value) && value >= 0;
  const utf8Length = (text) => encoder.encode(text).length;

  function toContent(type, payload = {}) {
    mount?.port.postMessage({ ...payload, type, generation: mount.generation });
  }

  function toHost(type, payload = {}) {
    if (mount) {
      send({ ...payload, type, generation: mount.generation });
    }
  }

  function applyHeight(height) {
    if (!mount) {
      return;
    }
    const bounded = Math.max(MIN_HEIGHT, Math.ceil(height));
    const shown = mount.expanded ? bounded : Math.min(bounded, mount.maxHeight);
    frame.style.height = mount.expanded ? "" : `${shown}px`;
    toHost("resize", { height: shown, contentHeight: bounded, clamped: bounded > shown });
  }

  function teardown() {
    if (!mount) {
      return;
    }
    clearTimeout(mount.suspendTimer);
    clearTimeout(mount.resizeTimer);
    mount.port.close();
    mount = null;
    frame.removeAttribute("src");
    frame.style.height = "0px";
  }

  function crashed(reason) {
    toHost("crashed", { reason });
    teardown();
  }

  function startMount(message) {
    const { generation, docUrl, title } = message;
    if (!isGeneration(generation) || typeof docUrl !== "string" || !docUrl.startsWith(docPrefix)) {
      return;
    }
    teardown();
    const channel = new MessageChannel();
    mount = {
      generation,
      port: channel.port1,
      maxHeight: isCount(message.maxHeight) && message.maxHeight >= MIN_HEIGHT ? message.maxHeight : DEFAULT_MAX_HEIGHT,
      expanded: message.expanded === true,
      loads: 0,
      lastHeight: MIN_HEIGHT,
      lastResizeAt: 0,
      resizeTimer: 0,
      lastSaveAt: 0,
      lastDraftAt: 0,
      lastErrorAt: 0,
      suspendTimer: 0,
    };
    const current = mount;
    document.body.classList.toggle("is-expanded", current.expanded);
    channel.port1.onmessage = (event) => {
      if (mount === current) {
        fromContent(event.data);
      }
    };
    frame.title = typeof title === "string" ? title.slice(0, 200) : "";
    frame.onload = () => {
      if (mount !== current) {
        return;
      }
      current.loads += 1;
      if (current.loads > 1) {
        // A second load means the content navigated or reloaded itself.
        crashed("reloaded");
        return;
      }
      frame.contentWindow?.postMessage({ type: "lingxi.connect", generation }, "*", [channel.port2]);
    };
    frame.style.height = `${MIN_HEIGHT}px`;
    frame.src = docUrl;
  }

  function reply(type, payload) {
    toContent(type, payload);
  }

  function fromContent(data) {
    if (!data || typeof data !== "object" || data.generation !== mount.generation) {
      return;
    }
    const now = Date.now();
    switch (data.type) {
      case "ready":
        toHost("ready");
        break;
      case "resize": {
        if (typeof data.height !== "number" || !Number.isFinite(data.height)) {
          return;
        }
        mount.lastHeight = data.height;
        const wait = RESIZE_INTERVAL_MS - (now - mount.lastResizeAt);
        clearTimeout(mount.resizeTimer);
        const current = mount;
        const run = () => {
          if (mount === current) {
            current.lastResizeAt = Date.now();
            applyHeight(current.lastHeight);
          }
        };
        if (wait <= 0) {
          run();
        } else {
          mount.resizeTimer = setTimeout(run, wait);
        }
        break;
      }
      case "state.save": {
        const { requestId, baseVersion, modelContent, privateContent } = data;
        if (!isCount(requestId) || !isCount(baseVersion) || typeof modelContent !== "string" || typeof privateContent !== "string") {
          return;
        }
        if (utf8Length(modelContent) + utf8Length(privateContent) > MAX_STATE_BYTES) {
          reply("state.rejected", { requestId, reason: "too_large" });
          return;
        }
        if (now - mount.lastSaveAt < SAVE_INTERVAL_MS) {
          reply("state.rejected", { requestId, reason: "rate_limited" });
          return;
        }
        mount.lastSaveAt = now;
        toHost("state.save", { requestId, baseVersion, modelContent, privateContent });
        break;
      }
      case "suspended":
        clearTimeout(mount.suspendTimer);
        toHost("suspended");
        break;
      case "expand.request":
        toHost("expand.request");
        break;
      case "followup.draft": {
        const text = typeof data.text === "string" ? data.text.trim() : "";
        if (text.length === 0 || [...text].length > MAX_DRAFT_CHARS) {
          reply("followup.rejected", { reason: "invalid" });
          return;
        }
        if (now - mount.lastDraftAt < DRAFT_INTERVAL_MS) {
          reply("followup.rejected", { reason: "rate_limited" });
          return;
        }
        mount.lastDraftAt = now;
        toHost("followup.draft", { text });
        break;
      }
      case "error": {
        if (now - mount.lastErrorAt < ERROR_INTERVAL_MS) {
          return;
        }
        mount.lastErrorAt = now;
        toHost("error", { message: String(data.message ?? "").slice(0, MAX_ERROR_CHARS) });
        break;
      }
      default:
        break;
    }
  }

  function receive(message) {
    if (!message || typeof message !== "object") {
      return;
    }
    if (message.type === "mount") {
      startMount(message);
      return;
    }
    if (!mount || message.generation !== mount.generation) {
      return;
    }
    switch (message.type) {
      case "theme":
        toContent("theme", { theme: message.theme });
        break;
      case "state.saved":
        toContent("state.saved", { requestId: message.requestId, version: message.version });
        break;
      case "state.rejected":
        toContent("state.rejected", {
          requestId: message.requestId,
          reason: String(message.reason ?? "rejected"),
          state: message.state ?? null,
        });
        break;
      case "suspend": {
        const current = mount;
        clearTimeout(current.suspendTimer);
        current.suspendTimer = setTimeout(() => {
          if (mount === current) {
            toHost("suspended", { timedOut: true });
          }
        }, SUSPEND_TIMEOUT_MS);
        toContent("suspend");
        break;
      }
      case "expanded":
        mount.expanded = message.expanded === true;
        document.body.classList.toggle("is-expanded", mount.expanded);
        toContent("expanded", { expanded: mount.expanded });
        applyHeight(mount.lastHeight);
        break;
      case "max-height":
        if (isCount(message.maxHeight) && message.maxHeight >= MIN_HEIGHT) {
          mount.maxHeight = message.maxHeight;
          applyHeight(mount.lastHeight);
        }
        break;
      case "unmount":
        teardown();
        break;
      default:
        break;
    }
  }

  send({ type: "shell.ready" });
})();
