// Runtime OS v2 is a direct cutover.  The host intentionally does not expose
// a v1 compatibility object to generated apps.
const BRIDGE_VERSION = "v2";

export function getLingXiBridge() {
  if (typeof window === "undefined") {
    return null;
  }

  const bridge = window.lingxi?.[BRIDGE_VERSION];
  return bridge && typeof bridge === "object" ? bridge : null;
}

export const FALLBACK_DEVICE_CONTEXT = {
  os: "unknown",
  formFactor: "unknown",
  viewport: { width: 0, height: 0 },
  safeArea: { top: 0, right: 0, bottom: 0, left: 0 },
  colorScheme: "light",
  reducedMotion: false,
  inputMode: "touch",
};

function nonNegativeNumber(value) {
  return typeof value === "number" && Number.isFinite(value) && value >= 0
    ? value
    : 0;
}

export function normalizeDeviceContext(value) {
  if (!value || typeof value !== "object") return FALLBACK_DEVICE_CONTEXT;
  return {
    os: typeof value.os === "string" ? value.os : "unknown",
    formFactor:
      typeof value.formFactor === "string" ? value.formFactor : "unknown",
    viewport: {
      width: nonNegativeNumber(value.viewport?.width),
      height: nonNegativeNumber(value.viewport?.height),
    },
    safeArea: {
      top: nonNegativeNumber(value.safeArea?.top),
      right: nonNegativeNumber(value.safeArea?.right),
      bottom: nonNegativeNumber(value.safeArea?.bottom),
      left: nonNegativeNumber(value.safeArea?.left),
    },
    colorScheme: ["dark", "light", "unknown"].includes(value.colorScheme)
      ? value.colorScheme
      : "unknown",
    reducedMotion: value.reducedMotion === true,
    inputMode: ["pointer", "touch", "hybrid", "unknown"].includes(
      value.inputMode,
    )
      ? value.inputMode
      : "unknown",
  };
}

export function getDeviceContext() {
  const bridge = getLingXiBridge();
  return normalizeDeviceContext(
    bridge?.deviceContext,
  );
}

/**
 * Query a declared collection.
 *
 * Filters are `{ fieldId, operator, value }`. The resolved page is
 * `{ records, nextOffset? }`; app fields live in `records[].document`, while
 * recordId, revision, createdAtMs, and updatedAtMs are host-owned metadata.
 */
export async function queryCollection(request) {
  const bridge = getLingXiBridge();
  if (!bridge?.data?.query) {
    throw new Error("LingXi data bridge is unavailable");
  }
  return bridge.data.query(request);
}

export async function mutateCollection(request) {
  const bridge = getLingXiBridge();
  if (!bridge?.data?.mutate) {
    throw new Error("LingXi data bridge is unavailable");
  }
  return bridge.data.mutate(request);
}

/** Upsert one complete document using the native tagged mutation contract. */
export async function upsertRecord(
  collection,
  recordId,
  document,
  expectedRevision,
) {
  const operation = { kind: "upsert", recordId, document };
  if (expectedRevision !== undefined) {
    operation.expectedRevision = expectedRevision;
  }
  return mutateCollection({ collection, operations: [operation] });
}

/** Delete one record using the native tagged mutation contract. */
export async function deleteRecord(collection, recordId, expectedRevision) {
  const operation = { kind: "delete", recordId };
  if (expectedRevision !== undefined) {
    operation.expectedRevision = expectedRevision;
  }
  return mutateCollection({ collection, operations: [operation] });
}

export async function requestNetwork(request) {
  const bridge = getLingXiBridge();
  const send = bridge?.network?.fetch;
  if (!send) {
    throw new Error("LingXi network bridge is unavailable");
  }
  return send(request);
}

export async function requestRuntimeStatus(request = {}) {
  const bridge = getLingXiBridge();
  const inspect =
    bridge?.runtime?.info;
  if (!inspect) {
    throw new Error("LingXi runtime bridge is unavailable");
  }
  return inspect(request);
}

function device(name) {
  const bridge = getLingXiBridge();
  const call = bridge?.device?.[name];
  if (!call) {
    throw new Error(`LingXi device bridge is unavailable (${name})`);
  }
  return call;
}

export async function capturePhoto(options = {}) {
  return device("capturePhoto")(options);
}

export async function pickImage(options = {}) {
  return device("pickImage")(options);
}

export async function startRecording(options = {}) {
  return device("recordAudioStart")(options);
}

export async function stopRecording() {
  return device("recordAudioStop")();
}

export async function getCurrentLocation() {
  return device("getLocation")();
}

export async function transcribeSpeech(options = {}) {
  return device("transcribeSpeech")(options);
}

export async function postNotification(request) {
  return device("postNotification")(request);
}

export async function getClipboardText() {
  const bridge = getLingXiBridge();
  if (!bridge?.clipboard?.getText) {
    throw new Error("LingXi clipboard bridge is unavailable");
  }
  return bridge.clipboard.getText();
}

export async function setClipboardText(text) {
  const bridge = getLingXiBridge();
  if (!bridge?.clipboard?.setText) {
    throw new Error("LingXi clipboard bridge is unavailable");
  }
  return bridge.clipboard.setText(text);
}

export async function shareContent(request) {
  return device("share")(request);
}

export async function synthesizeSpeech(request) {
  return device("synthesizeSpeech")(request);
}

export async function readFile(request) {
  const bridge = getLingXiBridge();
  if (!bridge?.files?.read) {
    throw new Error("LingXi file bridge is unavailable");
  }
  return bridge.files.read(request);
}

export async function writeFile(request) {
  const bridge = getLingXiBridge();
  if (!bridge?.files?.write) {
    throw new Error("LingXi file bridge is unavailable");
  }
  return bridge.files.write(request);
}

export async function getDeviceStatus() {
  return device("status")({});
}

export async function triggerHaptics(style) {
  return device("haptics")(style);
}

export async function openDeepLink(url) {
  return device("deepLink")(url);
}

export async function listCalendarEvents(request) {
  const bridge = getLingXiBridge();
  if (!bridge?.calendar?.listEvents) {
    throw new Error("LingXi calendar bridge is unavailable");
  }
  return bridge.calendar.listEvents(request);
}

export async function searchContacts(request) {
  const bridge = getLingXiBridge();
  if (!bridge?.contacts?.search) {
    throw new Error("LingXi contacts bridge is unavailable");
  }
  return bridge.contacts.search(request);
}

export async function getMedia(mediaId) {
  const bridge = getLingXiBridge();
  if (!bridge?.media?.get) {
    throw new Error("LingXi media bridge is unavailable");
  }
  return bridge.media.get({ mediaId });
}

export function mediaObjectURL(media) {
  const binary = atob(media.base64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) {
    bytes[i] = binary.charCodeAt(i);
  }
  return URL.createObjectURL(new Blob([bytes], { type: media.mimeType }));
}

export async function requestLlmChat(request) {
  const bridge = getLingXiBridge();
  if (!bridge?.llm?.chat) {
    throw new Error("LingXi AI bridge is unavailable");
  }
  return bridge.llm.chat(request);
}

/** Run a streaming side-query; subscribe with onLlmStreamFrame first. */
export async function streamLlmChat(request) {
  const bridge = getLingXiBridge();
  if (!bridge?.llm?.stream) {
    throw new Error("LingXi streaming AI bridge is unavailable");
  }
  return bridge.llm.stream(request);
}

export function onLlmStreamFrame(listener) {
  const bridge = getLingXiBridge();
  if (!bridge?.llm?.onFrame) {
    throw new Error("LingXi streaming AI bridge is unavailable");
  }
  return bridge.llm.onFrame(listener);
}

/** Subscribe only to frames from app-owned Agent streams. */
export function onAgentStreamFrame(listener) {
  const bridge = getLingXiBridge();
  if (!bridge?.agent?.onFrame) {
    throw new Error("LingXi Agent streaming bridge is unavailable");
  }
  return bridge.agent.onFrame(listener);
}

/** Post to the conversation Agent, or to `sessionId` for this app's Agent. */
export async function postAgentEvent(request) {
  const bridge = getLingXiBridge();
  if (!bridge?.agent?.post) {
    throw new Error("LingXi agent bridge is unavailable");
  }
  return bridge.agent.post(request);
}

/** Create a host-owned persistent Agent session for this app. */
export async function createAgentSession(request = {}) {
  const bridge = getLingXiBridge();
  if (!bridge?.agent?.sessions?.create) {
    throw new Error("LingXi Agent session bridge is unavailable");
  }
  return bridge.agent.sessions.create(request);
}

/** List this app's host-owned persistent Agent sessions. */
export async function listAgentSessions() {
  const bridge = getLingXiBridge();
  if (!bridge?.agent?.sessions?.list) {
    throw new Error("LingXi Agent session bridge is unavailable");
  }
  return bridge.agent.sessions.list();
}

export async function resumeAgentSession(request) {
  const bridge = getLingXiBridge();
  if (!bridge?.agent?.sessions?.resume) {
    throw new Error("LingXi Agent session bridge is unavailable");
  }
  return bridge.agent.sessions.resume(request);
}

export async function closeAgentSession(request) {
  const bridge = getLingXiBridge();
  if (!bridge?.agent?.sessions?.close) {
    throw new Error("LingXi Agent session bridge is unavailable");
  }
  return bridge.agent.sessions.close(request);
}

/** Run one non-streaming turn in an app-owned Agent session. */
export async function sendAgentTurn(request) {
  const bridge = getLingXiBridge();
  if (!bridge?.agent?.send) {
    throw new Error("LingXi Agent bridge is unavailable");
  }
  return bridge.agent.send(request);
}

/** Run one streaming turn in an app-owned Agent session. */
export async function streamAgentTurn(request) {
  const bridge = getLingXiBridge();
  if (!bridge?.agent?.stream) {
    throw new Error("LingXi Agent bridge is unavailable");
  }
  return bridge.agent.stream(request);
}

/** Cancel one in-flight app-owned Agent turn. */
export async function cancelAgentTurn(request) {
  const bridge = getLingXiBridge();
  if (!bridge?.agent?.cancel) {
    throw new Error("LingXi Agent bridge is unavailable");
  }
  return bridge.agent.cancel(request);
}

/** Propose, but never silently apply, an App Agent Profile update. */
export async function proposeAgentProfileUpdate(request) {
  const bridge = getLingXiBridge();
  if (!bridge?.agent?.profiles?.proposeUpdate) {
    throw new Error("LingXi Agent Profile bridge is unavailable");
  }
  return bridge.agent.profiles.proposeUpdate(request);
}

/** Register a bounded declarative flow with the host system scheduler. */
export async function scheduleBackgroundFlow(request) {
  const bridge = getLingXiBridge();
  if (!bridge?.background?.schedule) {
    throw new Error("LingXi background scheduler is unavailable");
  }
  return bridge.background.schedule(request);
}

/** List this app's bounded background task lifecycle and last results. */
export async function listBackgroundTasks(request = {}) {
  const bridge = getLingXiBridge();
  if (!bridge?.background?.list) {
    throw new Error("LingXi background task management is unavailable");
  }
  return bridge.background.list(request);
}

/** Read one app-owned background task lifecycle record and last result. */
export async function getBackgroundTaskStatus(request) {
  const bridge = getLingXiBridge();
  if (!bridge?.background?.status) {
    throw new Error("LingXi background task management is unavailable");
  }
  return bridge.background.status(request);
}

/** Cancel one app-owned background task. */
export async function cancelBackgroundTask(request) {
  const bridge = getLingXiBridge();
  if (!bridge?.background?.cancel) {
    throw new Error("LingXi background task management is unavailable");
  }
  return bridge.background.cancel(request);
}

/** Requeue one failed or cancelled app-owned background task. */
export async function retryBackgroundTask(request) {
  const bridge = getLingXiBridge();
  if (!bridge?.background?.retry) {
    throw new Error("LingXi background task management is unavailable");
  }
  return bridge.background.retry(request);
}
