/* Served by the run-scoped LingXi loopback gateway. No native message bridge. */
(() => {
  if (window.top !== window || window.localApp) return;
  const config = __LINGXI_APP_CONFIGURATION__;
  const agui = window.LingxiLocalAppAGUI;
  if (!agui || typeof agui.installBrowserAgent !== 'function') {
    throw new Error('The pinned AG-UI browser bundle must load before the Local App SDK.');
  }

  const base = location.origin + '/__run/' + encodeURIComponent(config.runId) + '/';
  const maxRequestBodyBytes = 116 * 1024 * 1024;
  const pendingTtlMs = 24 * 60 * 60 * 1000;
  const acceptedTtlMs = 7 * 24 * 60 * 60 * 1000;
  const maxPendingRecords = 32;
  const maxPendingStorageBytes = 100 * 1024 * 1024;
  const maxReportedMissingWakeRuns = 32;
  const pendingStore = 'pending';
  const pendingDbName = 'lingxi-local-app-agui-' + config.appId;
  const chatCreationDbName = 'lingxi-local-app-chat-creates-' + config.appId + '-revision-' + config.revision;
  const chatCreationStore = 'creations';
  const maxChatCreations = 64;
  const obsoletePendingDbName = 'lingxi-local-app-agent-' + config.appId;
  const pendingPrefix = config.appId + ':' + config.revision + ':';
  const readOnlyMethods = new Set([
    'agent.tasks.get', 'agent.tasks.list', 'agent.tasks.events',
    'agent.chats.get', 'agent.chats.list', 'agent.chats.history', 'agent.chats.creationStatus'
  ]);
  const inFlight = new Map();
  const chatCreateFlights = new Map();
  const handoffWaiters = new Map();
  const restoringWakeRuns = new Set();
  const reportedMissingWakeRuns = new Set();
  const chatIds = new Set();
  const chatMessageIds = new Map();
  const recoveryState = {status: 'checking'};
  const pendingChatRecovery = new Set();
  let pendingChatRecoveryRevision = 0;
  let hostRecoveryPanel = null;
  let hostRecoveryUnavailable = false;
  let hostRecoveryDismissed = false;
  let hostRecoveryAwaitingBody = false;
  const textEncoder = new TextEncoder();
  const hostResponded = Symbol('hostResponded');

  const makeError = (message, code = 'agent_error', cause) => {
    const error = new Error(message, cause ? {cause} : undefined);
    error.code = code;
    return error;
  };

  function isRecord(value) {
    if (!value || typeof value !== 'object' || Array.isArray(value)) return false;
    const prototype = Object.getPrototypeOf(value);
    return prototype === Object.prototype || prototype === null;
  }

  function canonical(value) {
    if (Array.isArray(value)) return value.map(canonical);
    if (isRecord(value)) {
      return Object.fromEntries(Object.keys(value).sort().map(key => [key, canonical(value[key])]));
    }
    return value;
  }

  function hex(bytes) {
    return [...new Uint8Array(bytes)].map(value => value.toString(16).padStart(2, '0')).join('');
  }

  async function digestStringInChunks(value) {
    if (!crypto.subtle) throw makeError('Secure request recovery is unavailable in this browser.', 'storage_unavailable');
    const chunkSize = 1024 * 1024;
    const digests = [];
    for (let offset = 0; offset < value.length; offset += chunkSize) {
      const bytes = textEncoder.encode(value.slice(offset, Math.min(offset + chunkSize, value.length)));
      digests.push(hex(await crypto.subtle.digest('SHA-256', bytes)));
    }
    return {length: value.length, chunks: digests};
  }

  function dataParts(input) {
    const parts = [];
    for (let messageIndex = 0; messageIndex < (input?.messages || []).length; messageIndex++) {
      const message = input.messages[messageIndex];
      if (!Array.isArray(message.content)) continue;
      for (let partIndex = 0; partIndex < message.content.length; partIndex++) {
        const part = message.content[partIndex];
        if ((part.type === 'image' || part.type === 'document') && part.source?.type === 'data') {
          parts.push({
            messageId: message.id,
            messageIndex,
            partId: part.id || null,
            partIndex,
            type: part.type,
            mimeType: part.source.mimeType,
            value: part.source.value
          });
        }
      }
    }
    return parts;
  }

  async function inputPayloadDigests(input) {
    const payloads = [];
    for (const part of dataParts(input)) {
      payloads.push({
        messageId: part.messageId,
        messageIndex: part.messageIndex,
        partId: part.partId,
        partIndex: part.partIndex,
        type: part.type,
        mimeType: part.mimeType,
        digest: await digestStringInChunks(part.value)
      });
    }
    return payloads;
  }

  function cloneWithoutInlinePayloads(value, lengths) {
    if (Array.isArray(value)) return value.map(item => cloneWithoutInlinePayloads(item, lengths));
    if (!isRecord(value)) return value;
    const result = {};
    for (const [key, item] of Object.entries(value)) {
      if (key === 'source' && isRecord(item) && item.type === 'data'
          && typeof item.value === 'string') {
        lengths.total += item.value.length;
        const source = {};
        for (const [sourceKey, sourceValue] of Object.entries(item)) {
          source[sourceKey] = sourceKey === 'value'
            ? ''
            : cloneWithoutInlinePayloads(sourceValue, lengths);
        }
        result[key] = source;
      } else {
        result[key] = cloneWithoutInlinePayloads(item, lengths);
      }
    }
    return result;
  }

  async function fingerprint(method, args) {
    const payloads = await inputPayloadDigests(args.input);
    const lengths = {total: 0};
    const metadata = cloneWithoutInlinePayloads(args, lengths);
    const bytes = textEncoder.encode(JSON.stringify(canonical({method, args: metadata, payloads})));
    return hex(await crypto.subtle.digest('SHA-256', bytes));
  }

  function inlineFileTotals(input, knownMessageIds) {
    let count = 0;
    let bytes = 0;
    for (const part of dataParts(input)) {
      if (knownMessageIds.has(part.messageId)) continue;
      count++;
      const value = part.value;
      const padding = value.endsWith('==') ? 2 : value.endsWith('=') ? 1 : 0;
      bytes += (value.length / 4) * 3 - padding;
    }
    return {count, bytes};
  }

  function utf8ByteLengthInChunks(value) {
    let length = 0;
    const chunkSize = 32 * 1024;
    for (let offset = 0; offset < value.length;) {
      let end = Math.min(offset + chunkSize, value.length);
      if (end < value.length) {
        const last = value.charCodeAt(end - 1);
        const next = value.charCodeAt(end);
        if (last >= 0xd800 && last <= 0xdbff && next >= 0xdc00 && next <= 0xdfff) end--;
      }
      length += textEncoder.encode(value.slice(offset, end)).byteLength;
      if (length > maxRequestBodyBytes) return length;
      offset = end;
    }
    return length;
  }

  function recordBytes(record) {
    if (Number.isSafeInteger(record.storageBytes) && record.storageBytes >= 0) return record.storageBytes;
    const copy = {...record};
    delete copy.storageBytes;
    const lengths = {total: 0};
    const metadata = cloneWithoutInlinePayloads(copy, lengths);
    return textEncoder.encode(JSON.stringify(metadata)).byteLength + lengths.total;
  }

  function database() {
    if (!('indexedDB' in window)) return Promise.reject(makeError('Persistent browser storage is unavailable.', 'storage_unavailable'));
    return new Promise((resolve, reject) => {
      const opening = indexedDB.open(pendingDbName, 1);
      opening.onupgradeneeded = () => {
        const db = opening.result;
        if (!db.objectStoreNames.contains(pendingStore)) db.createObjectStore(pendingStore, {keyPath: 'key'});
      };
      opening.onsuccess = () => resolve(opening.result);
      opening.onerror = () => reject(makeError('Could not open persistent browser storage.', 'storage_unavailable', opening.error));
      opening.onblocked = () => reject(makeError('Persistent browser storage is busy in another tab.', 'storage_unavailable'));
    });
  }

  // Chat creation is a LingXi host operation, separate from AG-UI run
  // submission. Each App revision retains its unresolved identity without TTL eviction.
  function chatCreationDatabase() {
    if (!('indexedDB' in window)) return Promise.reject(makeError('Persistent browser storage is unavailable.', 'storage_unavailable'));
    return new Promise((resolve, reject) => {
      const opening = indexedDB.open(chatCreationDbName, 1);
      opening.onupgradeneeded = () => {
        if (!opening.result.objectStoreNames.contains(chatCreationStore)) {
          opening.result.createObjectStore(chatCreationStore, {keyPath: 'key'});
        }
      };
      opening.onsuccess = () => resolve(opening.result);
      opening.onerror = () => reject(makeError('Could not open Chat recovery storage.', 'storage_unavailable', opening.error));
      opening.onblocked = () => reject(makeError('Chat recovery storage is busy in another tab.', 'storage_unavailable'));
    });
  }

  function chatCreationOptions(options) {
    if (!isRecord(options) || Object.keys(options).some(key => !['requestId', 'title'].includes(key))
        || typeof options.requestId !== 'string'
        || !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(options.requestId)
        || (Object.hasOwn(options, 'title') && (typeof options.title !== 'string'
            || options.title.length > 512
            || textEncoder.encode(options.title).byteLength > 512
            || /[\u0000-\u001f\u007f-\u009f]/u.test(options.title)
            || [...options.title].some(char => {
              const code = char.codePointAt(0);
              return code >= 0xd800 && code <= 0xdfff;
            })
            || !options.title.replace(/^\p{White_Space}+|\p{White_Space}+$/gu, '')
            || [...options.title.replace(/^\p{White_Space}+|\p{White_Space}+$/gu, '')].length > 80))) {
      throw makeError('Chat creation requires a UUIDv4 requestId and an optional bounded title.', 'invalid_chat');
    }
    return {requestId: options.requestId, ...(Object.hasOwn(options, 'title') ? {title: options.title} : {})};
  }

  function sameChatCreation(record, options) {
    return record?.options && typeof record.options === 'object' && !Array.isArray(record.options)
      && Object.keys(record.options).every(key => ['requestId', 'title'].includes(key))
      && record.appId === config.appId && record.revision === config.revision && record.key === options.requestId
      && record.requestId === options.requestId && record.options?.requestId === options.requestId
      && Object.hasOwn(record.options || {}, 'title') === Object.hasOwn(options, 'title')
      && record.options?.title === options.title;
  }

  function sameChatCreationOptions(left, right) {
    return left.requestId === right.requestId
      && Object.hasOwn(left, 'title') === Object.hasOwn(right, 'title')
      && left.title === right.title;
  }

  function validChatAttempt(record) {
    return typeof record.attemptToken === 'string' && /^[0-9a-f]{32}$/.test(record.attemptToken)
      && (!Object.hasOwn(record, 'observationToken')
        || (typeof record.observationToken === 'string' && /^[0-9a-f]{32}$/.test(record.observationToken)));
  }

  function newChatAttemptToken() {
    if (typeof crypto === 'undefined' || typeof crypto.getRandomValues !== 'function') {
      throw makeError('Secure Chat recovery is unavailable in this browser.', 'storage_unavailable');
    }
    return hex(crypto.getRandomValues(new Uint8Array(16)));
  }

  async function reserveChatCreation(options, allowExisting = false) {
    const db = await chatCreationDatabase();
    return new Promise((resolve, reject) => {
      const tx = db.transaction(chatCreationStore, 'readwrite');
      const store = tx.objectStore(chatCreationStore);
      let failure = null;
      let count = 0;
      let attemptToken = null;
      let alreadyExists = false;
      let existingState = null;
      const cursorRequest = store.openCursor();
      cursorRequest.onsuccess = () => {
        const cursor = cursorRequest.result;
        if (cursor) { count++; cursor.continue(); return; }
        const request = store.get(options.requestId);
        request.onsuccess = () => {
          const existing = request.result;
          if (existing) {
            if (!['pending', 'accepted', 'deleted', 'rejected'].includes(existing.state) || !validChatAttempt(existing)) {
              failure = makeError('Chat recovery data is invalid.', 'storage_unavailable');
            } else if (!sameChatCreation(existing, options)) {
              failure = makeError('This Chat creation requestId is already bound to different input.', 'chat_create_conflict');
            } else if (!allowExisting) {
              alreadyExists = true;
              existingState = existing.state;
            } else {
              // Fence every explicit attempt, including a retry of a pending
              // request, against older results from this or another tab.
              try {
                attemptToken = newChatAttemptToken();
                const next = {...existing, state: 'pending', attemptToken};
                delete next.observationToken;
                store.put(next);
              } catch (error) { failure = error; }
            }
            return;
          }
          if (count >= maxChatCreations) {
            failure = makeError('Chat recovery storage is full. Forget a resolved creation before continuing.', 'storage_full');
            return;
          }
          try {
            attemptToken = newChatAttemptToken();
            store.add({key: options.requestId, appId: config.appId, revision: config.revision, requestId: options.requestId,
              options, state: 'pending', attemptToken, createdAt: Date.now()});
          } catch (error) { failure = error; }
        };
      };
      tx.oncomplete = () => { db.close(); failure ? reject(failure) : resolve({alreadyExists, existingState, attemptToken}); };
      tx.onerror = () => { db.close(); reject(makeError('Could not save Chat creation for recovery.', 'storage_unavailable', tx.error)); };
      tx.onabort = () => { db.close(); reject(makeError('Could not save Chat creation for recovery.', 'storage_unavailable', tx.error)); };
    });
  }

  async function markChatCreationState(options, state, attemptToken, observationToken) {
    if (!attemptToken) return;
    const db = await chatCreationDatabase();
    return new Promise((resolve, reject) => {
      const tx = db.transaction(chatCreationStore, 'readwrite');
      const store = tx.objectStore(chatCreationStore);
      const request = store.get(options.requestId);
      request.onsuccess = () => {
        const record = request.result;
        if (!sameChatCreation(record, options) || !validChatAttempt(record) || record.attemptToken !== attemptToken) return;
        if (observationToken && record.observationToken !== observationToken) return;
        const allowed = record.state === 'pending'
          || (observationToken && state === 'deleted' && record.state === 'accepted');
        if (allowed) store.put({...record, state});
      };
      tx.oncomplete = () => { db.close(); resolve(); };
      tx.onerror = () => { db.close(); reject(makeError('Could not update Chat recovery storage.', 'storage_unavailable', tx.error)); };
      tx.onabort = () => { db.close(); reject(makeError('Could not update Chat recovery storage.', 'storage_unavailable', tx.error)); };
    });
  }

  async function readChatCreationRecords() {
    const db = await chatCreationDatabase();
    return new Promise((resolve, reject) => {
      const tx = db.transaction(chatCreationStore, 'readonly');
      const records = [];
      let failure = null;
      const request = tx.objectStore(chatCreationStore).openCursor();
      request.onsuccess = () => {
        const cursor = request.result;
        if (!cursor) return;
        const record = cursor.value;
        let valid = false;
        try {
          const stored = record?.options;
          if (!stored || typeof stored !== 'object' || Array.isArray(stored)
              || Object.keys(stored).some(key => !['requestId', 'title'].includes(key))) throw new Error('invalid receipt');
          const options = chatCreationOptions({requestId: stored.requestId,
            ...(Object.hasOwn(stored, 'title') ? {title: stored.title} : {})});
          valid = sameChatCreation(record, options)
            && ['pending', 'accepted', 'deleted', 'rejected'].includes(record.state)
            && validChatAttempt(record);
        } catch { /* report invalid recovery data below */ }
        if (!valid || records.length >= maxChatCreations) {
          failure = makeError('Chat recovery data is invalid.', 'storage_unavailable');
          return;
        } else {
          records.push({options: {...record.options}, state: record.state, attemptToken: record.attemptToken ?? null});
        }
        cursor.continue();
      };
      tx.oncomplete = () => { db.close(); failure ? reject(failure) : resolve(records); };
      tx.onerror = () => { db.close(); reject(makeError('Could not read Chat recovery storage.', 'storage_unavailable', tx.error)); };
      tx.onabort = () => { db.close(); reject(makeError('Could not read Chat recovery storage.', 'storage_unavailable', tx.error)); };
    });
  }

  async function pendingChatCreations() {
    const records = await readChatCreationRecords();
    return records.map(record => ({...record.options, state: record.state}));
  }

  // Capture a status observation before issuing it. An attempt token fences
  // newer creates; an observation token fences newer status reads, including
  // reads issued by another tab for the same attempt.
  async function captureChatObservation(options, expectedAttempt, pendingOnly = false) {
    const db = await chatCreationDatabase();
    return new Promise((resolve, reject) => {
      const tx = db.transaction(chatCreationStore, 'readwrite');
      const store = tx.objectStore(chatCreationStore);
      let observation = null;
      let failure = null;
      const request = store.get(options.requestId);
      request.onsuccess = () => {
        const record = request.result;
        if (!record || !sameChatCreation(record, options)
            || !['pending', 'accepted', 'deleted', 'rejected'].includes(record.state)
            || (pendingOnly && record.state !== 'pending')) return;
        if (!validChatAttempt(record)) {
          failure = makeError('Chat recovery data is invalid.', 'storage_unavailable');
          return;
        }
        const current = record.attemptToken ?? null;
        if (expectedAttempt !== undefined && current !== expectedAttempt) return;
        try {
          const attemptToken = current;
          const observationToken = newChatAttemptToken();
          store.put({...record, attemptToken, observationToken});
          observation = {attemptToken, observationToken};
        } catch (error) { failure = error; }
      };
      tx.oncomplete = () => { db.close(); failure ? reject(failure) : resolve(observation); };
      tx.onerror = () => { db.close(); reject(makeError('Could not read Chat recovery data.', 'storage_unavailable', tx.error)); };
      tx.onabort = () => { db.close(); reject(makeError('Could not read Chat recovery data.', 'storage_unavailable', tx.error)); };
    });
  }

  async function forgetChatCreation(requestId) {
    const options = chatCreationOptions({requestId});
    const db = await chatCreationDatabase();
    return new Promise((resolve, reject) => {
      const tx = db.transaction(chatCreationStore, 'readwrite');
      const store = tx.objectStore(chatCreationStore);
      let failure = null;
      const request = store.get(options.requestId);
      request.onsuccess = () => {
        const record = request.result;
        if (!record) return;
        let valid = false;
        try {
          const saved = record.options;
          if (!saved || typeof saved !== 'object' || Array.isArray(saved)
              || Object.keys(saved).some(key => !['requestId', 'title'].includes(key))) throw new Error('invalid receipt');
          const stored = chatCreationOptions({requestId: saved.requestId,
            ...(Object.hasOwn(saved, 'title') ? {title: saved.title} : {})});
          valid = sameChatCreation(record, stored) && validChatAttempt(record)
            && ['pending', 'accepted', 'deleted', 'rejected'].includes(record.state);
        } catch { /* retain malformed data */ }
        if (!valid) failure = makeError('Chat recovery data is invalid.', 'storage_unavailable');
        else if (['accepted', 'deleted', 'rejected'].includes(record.state)) store.delete(requestId);
        else failure = makeError('Only resolved Chat creations can be forgotten.', 'chat_creation_unresolved');
      };
      tx.oncomplete = () => { db.close(); failure ? reject(failure) : resolve(); };
      tx.onerror = () => { db.close(); reject(makeError('Could not remove Chat recovery data.', 'storage_unavailable', tx.error)); };
      tx.onabort = () => { db.close(); reject(makeError('Could not remove Chat recovery data.', 'storage_unavailable', tx.error)); };
    });
  }

  function deleteObsoletePendingDatabase() {
    if (!('indexedDB' in window)) return Promise.resolve(false);
    return new Promise(resolve => {
      try {
        const request = indexedDB.deleteDatabase(obsoletePendingDbName);
        request.onsuccess = () => resolve(true);
        request.onerror = () => resolve(false);
        request.onblocked = () => resolve(false);
      } catch {
        resolve(false);
      }
    });
  }

  function recordExpired(record, now = Date.now()) {
    const accepted = record?.state === 'accepted';
    const timestamp = accepted ? record.acceptedAt : record?.createdAt;
    const ttl = accepted ? acceptedTtlMs : pendingTtlMs;
    return !Number.isFinite(timestamp) || timestamp > now + 5 * 60 * 1000 || now - timestamp >= ttl;
  }

  async function collectExpired() {
    const db = await database();
    return new Promise((resolve, reject) => {
      const tx = db.transaction(pendingStore, 'readwrite');
      const store = tx.objectStore(pendingStore);
      const cursorRequest = store.openCursor();
      const readyRunIds = config.hostPlatform === 'ios'
        ? new URL(location.href).searchParams.getAll('__lingxi_ready') : [];
      const protectedWakeKey = readyRunIds.length === 1 && readyRunIds[0]
        ? requestKey(readyRunIds[0]) : null;
      let expired = 0;
      cursorRequest.onsuccess = () => {
        const cursor = cursorRequest.result;
        if (!cursor) return;
        const record = cursor.value;
        if (record?.appId === config.appId && record.key !== protectedWakeKey && recordExpired(record)) {
          cursor.delete();
          expired++;
        }
        cursor.continue();
      };
      tx.oncomplete = () => { db.close(); resolve({expired}); };
      tx.onerror = () => { db.close(); reject(makeError('Could not clear expired Agent recovery data.', 'storage_unavailable', tx.error)); };
      tx.onabort = () => { db.close(); reject(makeError('Could not clear expired Agent recovery data.', 'storage_unavailable', tx.error)); };
    });
  }

  async function writePending(record) {
    const db = await database();
    return new Promise((resolve, reject) => {
      const tx = db.transaction(pendingStore, 'readwrite');
      const store = tx.objectStore(pendingStore);
      const request = store.get(record.key);
      let failure = null;
      const next = {...record};
      delete next.storageBytes;
      next.storageBytes = recordBytes(next);
      request.onsuccess = () => {
        const existing = request.result;
        if (existing && (existing.appId !== config.appId || existing.revision !== next.revision
            || existing.runId !== next.runId || existing.fingerprint !== next.fingerprint
            || existing.method !== next.method)) {
          failure = makeError('This runId is already bound to different input.', 'run_id_conflict');
          return;
        }
        if (existing?.state === 'accepted' && next.state !== 'accepted') return;
        if (!existing && next.state !== 'accepted') {
          failure = makeError('The saved Agent run expired; create a new run after reviewing it.', 'run_expired');
          return;
        }
        if (existing && recordExpired(existing) && next.state !== 'accepted') {
          store.delete(next.key);
          failure = makeError('The saved Agent run expired; create a new run after reviewing it.', 'run_expired');
          return;
        }
        store.put(next);
      };
      tx.oncomplete = () => { db.close(); failure ? reject(failure) : resolve(); };
      tx.onerror = () => { db.close(); reject(makeError('Could not save the Agent run for recovery.', 'storage_unavailable', tx.error)); };
      tx.onabort = () => { db.close(); reject(makeError('Could not save the Agent run for recovery.', 'storage_unavailable', tx.error)); };
    });
  }

  async function reservePending(record) {
    const db = await database();
    return new Promise((resolve, reject) => {
      const tx = db.transaction(pendingStore, 'readwrite');
      const store = tx.objectStore(pendingStore);
      let result = null;
      let conflict = false;
      let failure = null;
      let liveCount = 0;
      let liveBytes = 0;
      const cursorRequest = store.openCursor();
      cursorRequest.onsuccess = () => {
        const cursor = cursorRequest.result;
        if (cursor) {
          const existingRecord = cursor.value;
          if (existingRecord?.appId === config.appId) {
            if (recordExpired(existingRecord)) cursor.delete();
            else { liveCount++; liveBytes += recordBytes(existingRecord); }
          }
          cursor.continue();
          return;
        }
        const request = store.get(record.key);
        request.onsuccess = () => {
          const existing = request.result;
          if (existing) {
            conflict = existing.appId !== config.appId || existing.revision !== record.revision
              || existing.runId !== record.runId || existing.fingerprint !== record.fingerprint
              || existing.method !== record.method;
            if (conflict) return;
            if (existing.state === 'accepted') { result = existing; return; }
            if (existing.state === 'rejected') {
              const projectedBytes = liveBytes - recordBytes(existing) + recordBytes(record);
              if (projectedBytes > maxPendingStorageBytes) {
                failure = makeError('Browser recovery storage is full. Forget an old run and try again.', 'storage_full');
                return;
              }
              store.put(record);
              result = record;
              return;
            }
            result = existing;
            return;
          }
          if (liveCount + 1 > maxPendingRecords || liveBytes + recordBytes(record) > maxPendingStorageBytes) {
            failure = makeError('Browser recovery storage is full. Forget an old run and try again.', 'storage_full');
            return;
          }
          store.add(record);
          result = record;
        };
      };
      tx.oncomplete = () => {
        db.close();
        if (conflict) reject(makeError('This runId is already bound to different input.', 'run_id_conflict'));
        else if (failure) reject(failure);
        else resolve(result);
      };
      tx.onerror = () => { db.close(); reject(makeError('Could not save the Agent run for recovery.', 'storage_unavailable', tx.error)); };
      tx.onabort = () => { db.close(); reject(makeError('Could not save the Agent run for recovery.', 'storage_unavailable', tx.error)); };
    });
  }

  async function claimPending(key, runId) {
    const db = await database();
    return new Promise((resolve, reject) => {
      const tx = db.transaction(pendingStore, 'readwrite');
      const store = tx.objectStore(pendingStore);
      const request = store.get(key);
      let outcome = {status: 'ineligible'};
      request.onsuccess = () => {
        const record = request.result;
        if (record === undefined) {
          outcome = {status: 'missing'};
          return;
        }
        if (record && recordExpired(record)) {
          store.delete(key);
          return;
        }
        if (record && record.key === key && record.runId === runId
            && record.appId === config.appId && record.revision === config.revision) {
          if (record.state === 'accepted') {
            outcome = {status: 'accepted', record};
          } else if (record.state === 'waking') {
            record.state = 'sending';
            record.sendingAt = Date.now();
            record.storageBytes = recordBytes(record);
            store.put(record);
            outcome = {status: 'claimed', record};
          }
        }
      };
      tx.oncomplete = () => { db.close(); resolve(outcome); };
      tx.onerror = () => { db.close(); reject(makeError('Could not restore the saved Agent run.', 'storage_unavailable', tx.error)); };
      tx.onabort = () => { db.close(); reject(makeError('Could not restore the saved Agent run.', 'storage_unavailable', tx.error)); };
    });
  }

  function renderHostRecovery() {
    if (!hostRecoveryUnavailable && pendingChatRecovery.size === 0) return;
    if (typeof document === 'undefined' || typeof document.createElement !== 'function') return;
    if (!document.body) {
      if (!hostRecoveryAwaitingBody && typeof document.addEventListener === 'function') {
        hostRecoveryAwaitingBody = true;
        try {
          document.addEventListener('DOMContentLoaded', () => {
            hostRecoveryAwaitingBody = false;
            renderHostRecovery();
          }, {once: true});
        } catch { hostRecoveryAwaitingBody = false; }
      }
      return;
    }
    try {
      if (!hostRecoveryPanel || hostRecoveryPanel.container.isConnected === false) {
        const container = document.createElement('div');
        container.style.cssText = 'position:fixed;z-index:2147483647;left:16px;right:16px;bottom:max(16px,env(safe-area-inset-bottom));';
        const root = typeof container.attachShadow === 'function' ? container.attachShadow({mode: 'closed'}) : container;
        const panel = document.createElement('section');
        panel.setAttribute('role', 'status');
        panel.setAttribute('aria-live', 'polite');
        panel.setAttribute('aria-label', 'LingXi connection recovery');
        panel.style.cssText = 'box-sizing:border-box;max-width:36rem;margin:auto;padding:14px;border:1px solid #777;border-radius:12px;background:#fff;color:#111;box-shadow:0 3px 16px #0004;font:16px/1.4 system-ui,sans-serif;';
        const message = document.createElement('p');
        message.style.cssText = 'margin:0 0 8px;';
        const open = document.createElement('button');
        open.type = 'button';
        open.textContent = 'Open LingXi';
        open.style.cssText = 'min-height:44px;padding:8px 12px;margin-right:8px;cursor:pointer;';
        open.addEventListener('click', event => {
          // Browser-generated clicks include keyboard and accessibility actions;
          // synthetic page events must never initiate the return navigation.
          if (!event.isTrusted) return;
          try { host.open(); }
          catch { message.textContent = 'Could not open LingXi. Switch to LingXi and reopen this App.'; }
        });
        const dismiss = document.createElement('button');
        dismiss.type = 'button';
        dismiss.textContent = 'Minimize';
        dismiss.style.cssText = 'min-height:44px;padding:8px 12px;cursor:pointer;';
        dismiss.addEventListener('click', event => {
          if (!event.isTrusted) return;
          hostRecoveryDismissed = true;
          renderHostRecovery();
        });
        const expand = document.createElement('button');
        expand.type = 'button';
        expand.textContent = 'Show recovery options';
        expand.style.cssText = 'min-height:44px;padding:8px 12px;cursor:pointer;';
        expand.addEventListener('click', event => {
          if (!event.isTrusted) return;
          hostRecoveryDismissed = false;
          renderHostRecovery();
        });
        const retries = document.createElement('div');
        panel.append(message, open, dismiss, expand, retries);
        root.append(panel);
        document.body.append(container);
        hostRecoveryPanel = {container, message, dismiss, expand, retries, retryRevision: -1};
      }
      hostRecoveryPanel.message.hidden = hostRecoveryDismissed;
      hostRecoveryPanel.dismiss.hidden = hostRecoveryDismissed;
      hostRecoveryPanel.expand.hidden = !hostRecoveryDismissed;
      hostRecoveryPanel.retries.hidden = hostRecoveryDismissed;
      const messageText = [...pendingChatRecovery].some(attempt => attempt.confirmRetry)
        ? 'A previous Chat creation may still complete. Confirm before sending the same request again.'
        : hostRecoveryUnavailable
          ? 'LingXi is unavailable or paused. Open LingXi, then reopen this App. Earlier requests may still complete.'
          : 'Chat creation is still waiting. It may already have been received. Open LingXi if the host is paused.';
      if (hostRecoveryPanel.message.textContent !== messageText) hostRecoveryPanel.message.textContent = messageText;
      if (hostRecoveryPanel.retryRevision !== pendingChatRecoveryRevision) {
        hostRecoveryPanel.retries.replaceChildren();
        hostRecoveryPanel.retryAttempts = new Set();
        for (const attempt of pendingChatRecovery) {
          const retry = document.createElement('button');
          retry.type = 'button';
          // The opaque request ID identifies the action; no title or input is shown.
          retry.textContent = 'Retry Chat creation ' + attempt.options.requestId;
          retry.style.cssText = 'min-height:44px;padding:8px 12px;cursor:pointer;';
          retry.addEventListener('click', event => {
            if (!event.isTrusted || !pendingChatRecovery.has(attempt)) return;
            if (attempt.confirmRetry) {
              const confirm = attempt.confirmRetry;
              attempt.confirmRetry = null;
              clearChatCreationPending(attempt);
              confirm.resolve();
            } else if (attempt.cancelWaiting && !attempt.retryRequested) {
              attempt.retryRequested = true;
              attempt.cancelWaiting();
            }
          });
          hostRecoveryPanel.retries.append(retry);
          hostRecoveryPanel.retryAttempts.add(attempt);
          if (attempt.confirmRetry) {
            const cancel = document.createElement('button');
            cancel.type = 'button';
            cancel.textContent = 'Cancel retry ' + attempt.options.requestId;
            cancel.style.cssText = 'min-height:44px;padding:8px 12px;cursor:pointer;';
            cancel.addEventListener('click', event => {
              if (!event.isTrusted || !attempt.confirmRetry) return;
              const confirm = attempt.confirmRetry;
              attempt.confirmRetry = null;
              clearChatCreationPending(attempt);
              confirm.reject(makeError('The Chat creation retry was cancelled.', 'chat_retry_cancelled'));
            });
            hostRecoveryPanel.retries.append(cancel);
          }
        }
        hostRecoveryPanel.retryRevision = pendingChatRecoveryRevision;
      }
    } catch {
      // A restricted or absent DOM must not change the Agent operation's result.
    }
  }

  function noteHostUnavailable() {
    hostRecoveryUnavailable = true;
    renderHostRecovery();
  }

  function removeHostRecoveryPanel() {
    try { hostRecoveryPanel?.container.remove(); } catch { /* UI is best effort */ }
    hostRecoveryPanel = null;
  }

  function noteHostReachable() {
    hostRecoveryUnavailable = false;
    if (pendingChatRecovery.size === 0) {
      hostRecoveryDismissed = false;
      removeHostRecoveryPanel();
    } else {
      renderHostRecovery();
    }
  }

  function noteChatCreationPending(attempt) {
    if (!pendingChatRecovery.has(attempt)) {
      pendingChatRecovery.add(attempt);
      pendingChatRecoveryRevision++;
    }
    renderHostRecovery();
  }

  function clearChatCreationPending(attempt) {
    if (pendingChatRecovery.delete(attempt)) pendingChatRecoveryRevision++;
    if (!hostRecoveryUnavailable && pendingChatRecovery.size === 0) {
      hostRecoveryDismissed = false;
      removeHostRecoveryPanel();
    } else {
      renderHostRecovery();
    }
  }

  function confirmChatCreationRetry(attempt) {
    if (typeof document === 'undefined' || !document.body || typeof document.createElement !== 'function') {
      return Promise.reject(makeError('A visible user confirmation is required to retry Chat creation.', 'chat_retry_confirmation_required'));
    }
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        const confirm = attempt.confirmRetry;
        if (!confirm) return;
        attempt.confirmRetry = null;
        clearChatCreationPending(attempt);
        const error = makeError('Chat retry confirmation expired; the earlier outcome is still unknown.', 'chat_retry_confirmation_timeout');
        error.definitive = false;
        confirm.reject(error);
      }, 120000);
      attempt.confirmRetry = {
        resolve() { clearTimeout(timer); resolve(); },
        reject(error) { clearTimeout(timer); reject(error); }
      };
      // A new explicit call can expose the confirmation even if a previous
      // outage was minimized; background failures never expand it themselves.
      hostRecoveryDismissed = false;
      noteChatCreationPending(attempt);
      if (!hostRecoveryPanel?.retryAttempts?.has(attempt)) {
        const confirm = attempt.confirmRetry;
        attempt.confirmRetry = null;
        clearChatCreationPending(attempt);
        confirm.reject(makeError('A visible user confirmation is required to retry Chat creation.', 'chat_retry_confirmation_required'));
      }
    });
  }

  async function invoke(method, args = {}, {cancelWaiting = null, silentRecovery = false} = {}) {
    const body = JSON.stringify({method, args});
    if (utf8ByteLengthInChunks(body) > maxRequestBodyBytes) {
      throw makeError('The AG-UI request exceeds the 116 MiB transport limit.', 'invalid_input');
    }
    const controller = new AbortController();
    const transportNow = () => {
      try {
        const clock = globalThis.performance;
        const value = typeof clock?.now === 'function' ? clock.now() : undefined;
        if (Number.isFinite(value)) return {source: 'performance', value};
      } catch { /* Diagnostic clocks must not prevent an Agent request. */ }
      try {
        const value = Date.now();
        if (Number.isFinite(value)) return {source: 'date', value};
      } catch { /* A missing diagnostic timestamp is safe. */ }
      return null;
    };
    const transportStartedAt = transportNow();
    let transportPhase = 'request';
    let transportStatus;
    // Local Error metadata only. Never add these fields to AG-UI messages,
    // events or results, and never include caller identities or request content.
    const annotateTransport = (error, reason) => {
      try {
        let visibility = 'unknown';
        try {
          const current = typeof document === 'undefined' ? undefined : document?.visibilityState;
          if (current === 'visible' || current === 'hidden') visibility = current;
        } catch { /* Page visibility is optional diagnostic context. */ }
        const finishedAt = transportNow();
        const elapsed = transportStartedAt && finishedAt && transportStartedAt.source === finishedAt.source
          ? finishedAt.value - transportStartedAt.value : undefined;
        const elapsedMs = Number.isFinite(elapsed) && elapsed >= 0
          ? Math.min(120000, Math.round(elapsed)) : undefined;
        error.transport = Object.freeze({
          phase: transportPhase,
          reason,
          ...(transportStatus === undefined ? {} : {httpStatus: transportStatus}),
          ...(elapsedMs === undefined ? {} : {elapsedMs}),
          visibility
        });
      } catch { /* Diagnostics must never replace the operation's error. */ }
      return error;
    };
    const transportError = (message, code, reason, cause) => annotateTransport(makeError(message, code, cause), reason);
    const isSubmission = method === 'agent.tasks.start' || method === 'agent.chats.send';
    const unavailableMessage = isSubmission
      ? 'LingXi is unavailable or paused. The run is saved for an explicit retry.'
      : 'LingXi is unavailable or paused; open LingXi to reconnect.';
    let timer;
    const deadline = new Promise((_, reject) => {
      timer = setTimeout(() => {
        // Settle the deadline before aborting: an abort may synchronously
        // reject fetch or body reading, but the timeout remains a soft error.
        const error = makeError(unavailableMessage, 'host_unavailable');
        reject(error);
        controller.abort();
        annotateTransport(error, 'timeout');
      }, readOnlyMethods.has(method) ? 5000 : 120000);
    });
    try {
      const requests = [deadline, (async () => {
        let response;
        try {
          response = await fetch(base + '__host/agent', {
            method: 'POST',
            credentials: 'omit',
            cache: 'no-store',
            redirect: 'error',
            headers: {
              'Content-Type': 'application/json',
              'X-LingXi-App-Request': '1'
            },
            body,
            signal: controller.signal
          });
        } catch (cause) {
          throw transportError(unavailableMessage, 'host_unavailable', 'fetch_failed', cause);
        }
        transportPhase = 'http_response';
        try {
          const status = response.status;
          if (Number.isInteger(status) && status >= 100 && status <= 599) transportStatus = status;
        } catch { /* HTTP status is diagnostic only. */ }
        if (!response.ok) {
          controller.abort();
          const error = transportError('The application run is unavailable. Return to LingXi to resume.', 'host_unavailable', 'http_error');
          error.definitive = true;
          throw error;
        }
        let result;
        transportPhase = 'response_body';
        try { result = await response.json(); }
        catch (cause) {
          if (cause?.name === 'SyntaxError') {
            throw transportError('The host returned an invalid Agent response.', 'invalid_host_response', 'invalid_json', cause);
          }
          throw transportError(unavailableMessage, 'host_unavailable', 'body_failed', cause);
        }
        transportPhase = 'host_response';
        if (!result?.ok) {
          const detail = result?.error || {};
          const error = transportError(detail.message || 'Agent request failed.', detail.code || 'agent_error', 'host_error');
          error.definitive = true;
          error[hostResponded] = true;
          throw error;
        }
        return result.value;
      })()];
      if (cancelWaiting) requests.push(cancelWaiting.then(() => {
        // Aborting the browser wait does not prove the host did not create it.
        const error = makeError('The earlier request may still complete in LingXi.', 'host_unavailable');
        error.definitive = false;
        controller.abort();
        throw annotateTransport(error, 'wait_cancelled');
      }));
      const result = await Promise.race(requests);
      if (!silentRecovery && (readOnlyMethods.has(method) || method === 'agent.chats.create')) noteHostReachable();
      return result;
    } catch (error) {
      if (!silentRecovery && error?.[hostResponded] && error.code !== 'host_unavailable'
          && (readOnlyMethods.has(method) || method === 'agent.chats.create')) noteHostReachable();
      if (!silentRecovery && error?.code === 'host_unavailable'
          && (readOnlyMethods.has(method) || method === 'agent.chats.create')) {
        noteHostUnavailable();
      }
      throw error;
    } finally {
      clearTimeout(timer);
    }
  }

  function requestKey(runId) { return pendingPrefix + runId; }

  async function storeRequest(method, extraArgs, input, knownMessageIds) {
    const args = {...extraArgs, input};
    const digest = await fingerprint(method, args);
    const uploads = inlineFileTotals(input, knownMessageIds);
    const runId = input.runId;
    const key = requestKey(runId);
    const record = {
      key,
      appId: config.appId,
      revision: config.revision,
      runId,
      threadId: input.threadId,
      method,
      chatId: method === 'agent.chats.send' ? input.threadId : undefined,
      input,
      knownMessageIds: [...knownMessageIds],
      uploads,
      fingerprint: digest,
      state: 'pending',
      createdAt: Date.now()
    };
    record.storageBytes = recordBytes(record);
    return await reservePending(record);
  }

  async function finishAccepted(record) {
    const accepted = {key: record.key, appId: record.appId, revision: record.revision,
      runId: record.runId, threadId: record.threadId, method: record.method,
      fingerprint: record.fingerprint, state: 'accepted', acceptedAt: Date.now()};
    await writePending(accepted);
    return {runId: record.runId};
  }

  function waitForHandoff(record, threadId, method) {
    const runId = record.runId;
    let waiter;
    const promise = new Promise((resolve, reject) => {
      const timeout = setTimeout(() => {
        handoffWaiters.delete(runId);
        reject(makeError('Return to this app after LingXi confirms the Agent handoff.', 'handoff_timeout'));
      }, 120000);
      waiter = {
        identity: {key: requestKey(runId), appId: config.appId, revision: config.revision,
          runId, threadId, method, fingerprint: record.fingerprint},
        promise: null,
        resolve(value) { clearTimeout(timeout); handoffWaiters.delete(runId); resolve(value); },
        reject(error) { clearTimeout(timeout); handoffWaiters.delete(runId); reject(error); }
      };
      handoffWaiters.set(runId, waiter);
    });
    waiter.promise = promise;
    return promise;
  }

  function wakeLink(runId) {
    const url = new URL('lingxi://apps/' + encodeURIComponent(config.appId));
    url.searchParams.set('review', 'agent');
    url.searchParams.set('runId', runId);
    url.searchParams.set('revision', String(config.revision));
    return url.href;
  }

  async function submitRecord(record) {
    if (record.state === 'accepted' && record.runId) {
      return {runId: record.runId};
    }
    if (inFlight.has(record.key)) return inFlight.get(record.key);
    const promise = (async () => {
      const active = {...record, state: 'sending', sendingAt: Date.now()};
      await writePending(active);
      try {
        const args = record.method === 'agent.chats.send'
          ? {chatId: record.chatId, input: record.input}
          : {input: record.input};
        const value = await invoke(record.method, args);
        if (!value || value.runId !== record.runId) {
          throw makeError('The host did not preserve the AG-UI runId.', 'invalid_host_response');
        }
        const accepted = await finishAccepted(record);
        return accepted;
      } catch (error) {
        if (error.definitive) {
          await writePending({key: record.key, appId: record.appId, revision: record.revision,
            runId: record.runId, threadId: record.threadId, method: record.method,
            fingerprint: record.fingerprint, state: 'rejected', createdAt: record.createdAt,
            lastError: error.code || 'agent_rejected'});
        } else {
          await writePending({...record, state: 'uncertain', lastError: error.code || 'host_unavailable', lastAttemptAt: Date.now()});
        }
        throw error;
      }
    })();
    inFlight.set(record.key, promise);
    try { return await promise; }
    finally { inFlight.delete(record.key); }
  }

  async function startInput(method, extraArgs, input, knownMessageIds = new Set()) {
    const persistentChat = method === 'agent.chats.send';
    if (method === 'agent.tasks.start' && chatIds.has(input?.threadId)) {
      throw makeError('This thread belongs to a persistent chat; use its chat handle.', 'chat_thread_required');
    }
    if (persistentChat && input?.threadId !== extraArgs.chatId) {
      throw new TypeError('A chat RunAgentInput.threadId must equal the chat handle id.');
    }
    const normalized = agui.validateLocalAppRunInput(input, {persistentChat, knownMessageIds});
    const record = await storeRequest(method, extraArgs, normalized, knownMessageIds);
    if (record.state === 'accepted' && record.runId) return {runId: record.runId};
    if (inFlight.has(record.key)) return inFlight.get(record.key);
    if (config.hostPlatform === 'ios') {
      if (record.state !== 'waking') {
        await writePending({...record, state: 'waking', wakeRequestedAt: Date.now()});
      }
      const waiter = handoffWaiters.get(normalized.runId);
      if (waiter) return waiter.promise;
      const pending = waitForHandoff(record, normalized.threadId, method);
      try { location.assign(wakeLink(normalized.runId)); }
      catch (cause) {
        handoffWaiters.get(normalized.runId)?.reject(makeError('Could not open LingXi for the Agent handoff.', 'handoff_unavailable', cause));
      }
      return pending;
    }
    return submitRecord(record);
  }

  function hostEventError(runId, error) {
    window.dispatchEvent(new CustomEvent('localApp:agent-events-error', {
      detail: {runId, code: error?.code || 'host_unavailable', message: error?.message || 'Could not read AG-UI events.'}
    }));
  }

  async function taskEvents(input) {
    if (!isRecord(input) || typeof input.runId !== 'string' || !input.runId) {
      throw new TypeError('tasks.events requires a runId.');
    }
    if (input.afterSequence !== undefined && (!Number.isSafeInteger(input.afterSequence) || input.afterSequence < 0)) {
      throw new TypeError('afterSequence must be a nonnegative safe integer.');
    }
    const page = await invoke('agent.tasks.events', input);
    if (!isRecord(page) || !Array.isArray(page.events) || !Number.isSafeInteger(page.sequence) || page.sequence < 0) {
      throw makeError('The host returned an invalid AG-UI event page.', 'invalid_host_response');
    }
    return {...page, events: page.events.map(event => agui.validateEvent(event))};
  }

  function subscribeTaskEvents(runId, listener) {
    if (typeof listener !== 'function') throw new TypeError('A listener is required.');
    let active = true;
    let timer = null;
    let afterSequence;
    const terminal = task => ['succeeded', 'failed', 'cancelled', 'interrupted'].includes(task?.state);
    const poll = async () => {
      let page;
      try {
        page = await taskEvents(afterSequence === undefined ? {runId} : {runId, afterSequence});
      } catch (error) {
        if (!active) return;
        hostEventError(runId, error);
        if (!active) return;
        try {
          const task = await invoke('agent.tasks.get', {runId});
          if (!active) return;
          // A transient transport failure can recover even after task completion.
          // A terminal task whose event log is unreadable must not poll forever.
          if (terminal(task) && error?.code !== 'host_unavailable') {
            active = false;
            return;
          }
        } catch {
          // Without an authoritative task state, retry at the normal polling interval.
        }
        if (active) timer = setTimeout(poll, 1000);
        return;
      }
      if (!active) return;
      try {
        for (const event of page.events) {
          listener(event);
          if (!active) return;
          if (event.type === 'RUN_FINISHED' || event.type === 'RUN_ERROR') {
            active = false;
            clearTimeout(timer);
            return;
          }
        }
        afterSequence = page.sequence;
        const task = await invoke('agent.tasks.get', {runId});
        if (!active) return;
        if (terminal(task)) {
          hostEventError(runId, makeError('The host task ended without a terminal AG-UI event.', 'missing_terminal_event'));
          active = false;
          return;
        }
      } catch (error) {
        if (active) hostEventError(runId, error);
      }
      if (active) timer = setTimeout(poll, 1000);
    };
    void poll();
    return () => { active = false; clearTimeout(timer); };
  }

  function requestChatHandle(summary, expectedId) {
    const id = summary?.chatId;
    if (typeof id !== 'string' || !id || (expectedId !== undefined && id !== expectedId)) {
      throw makeError('The host returned an invalid Chat ID.', 'invalid_host_response');
    }
    chatIds.add(id);
    if (!chatMessageIds.has(id)) chatMessageIds.set(id, new Set());
    const knownMessages = chatMessageIds.get(id);
    const handle = {
      id,
      async send(input) {
        if (input?.threadId !== id) throw new TypeError('A chat RunAgentInput.threadId must equal the chat handle id.');
        return agent.run(input);
      },
      async history(options = {}) {
        if (!isRecord(options) || Object.keys(options).some(key => !['cursor', 'limit'].includes(key))) {
          throw new TypeError('Chat history options may contain only cursor and limit.');
        }
        const page = await invoke('agent.chats.history', {...options, chatId: id});
        if (!isRecord(page) || !Array.isArray(page.messages)
            || (page.nextCursor !== undefined && typeof page.nextCursor !== 'string')) {
          throw makeError('The host returned invalid chat history.', 'invalid_host_response');
        }
        const messages = page.messages.map(message => agui.validateMessage(message));
        for (const message of messages) knownMessages.add(message.id);
        return {...page, messages};
      },
      subscribe(listener) {
        if (typeof listener !== 'function') throw new TypeError('A listener is required.');
        let active = true;
        let timer = null;
        let cursor;
        const seen = new Set();
        const poll = async () => {
          try {
            let page = await handle.history({limit: 50, ...(cursor ? {cursor} : {})});
            while (active) {
              for (const message of page.messages) {
                if (!seen.has(message.id)) {
                  seen.add(message.id);
                  listener(message);
                }
              }
              if (!page.nextCursor) {
                if (page.messages.length) cursor = page.messages[page.messages.length - 1].id;
                break;
              }
              cursor = page.nextCursor;
              page = await handle.history({cursor, limit: 50});
            }
          } catch (error) {
            if (active) window.dispatchEvent(new CustomEvent('localApp:chat-history-error', {
              detail: {chatId: id, code: error.code || 'host_unavailable', message: error.message}
            }));
          }
          if (active) timer = setTimeout(poll, 1000);
        };
        void poll();
        return () => { active = false; clearTimeout(timer); };
      },
      async cancel() { await invoke('agent.chats.cancel', {chatId: id}); }
    };
    return Object.freeze(handle);
  }

  const tasks = Object.freeze({
    start(input) {
      const known = chatMessageIds.get(input?.threadId) || new Set();
      return startInput('agent.tasks.start', {}, input, known);
    },
    get(runId) { return invoke('agent.tasks.get', {runId}); },
    list(options) { return invoke('agent.tasks.list', options || {}); },
    events: taskEvents,
    subscribe: subscribeTaskEvents,
    async cancel(runId) { await invoke('agent.tasks.cancel', {runId}); }
  });

  let agent;
  async function createChat(rawOptions) {
    const options = chatCreationOptions(rawOptions);
    const existing = chatCreateFlights.get(options.requestId);
    if (existing) {
      if (!sameChatCreationOptions(existing.options, options)) {
        throw makeError('This Chat creation requestId is already bound to different input.', 'chat_create_conflict');
      }
      return existing.promise;
    }
    const attempt = {options, promise: null, cancelWaiting: null, retryRequested: false, confirmRetry: null};
    const flight = (async () => {
      let reservation = await reserveChatCreation(options);
      if (reservation.alreadyExists) {
        if (reservation.existingState === 'deleted') {
          const error = makeError('This Chat creation was deleted and cannot be recreated.', 'chat_create_deleted');
          error.definitive = true;
          throw error;
        }
        let statusError = null;
        try {
          const known = await chatCreationStatus(options);
          if (known) return known;
        } catch (error) {
          if (!['host_unavailable', 'invalid_host_response', 'chat_storage_uncertain'].includes(error?.code)) throw error;
          statusError = error;
        }
        if (reservation.existingState !== 'pending') {
          // A previously resolved receipt does not authorize another POST
          // merely because a later read cannot currently find the resource.
          if (statusError) throw statusError;
          throw makeError('The previously resolved Chat creation cannot currently be verified.', 'chat_storage_uncertain');
        }
        await confirmChatCreationRetry(attempt);
        reservation = await reserveChatCreation(options, true);
      }
      let attemptToken = reservation.attemptToken;
      for (;;) {
        let cancel;
        const cancelWaiting = new Promise(resolve => { cancel = resolve; });
        attempt.cancelWaiting = cancel;
        const timer = setTimeout(() => {
          noteChatCreationPending(attempt);
          window.dispatchEvent(new CustomEvent('localApp:chat-creation-pending', {
            detail: {requestId: options.requestId}
          }));
        }, 5000);
        try {
          const summary = await invoke('agent.chats.create', options, {cancelWaiting});
          const handle = requestChatHandle(summary, options.requestId);
          // A newer cross-tab attempt must retain its own pending receipt.
          try { await markChatCreationState(options, 'accepted', attemptToken); } catch { /* retain pending */ }
          return handle;
        } catch (error) {
          if (attempt.retryRequested && error?.code === 'host_unavailable') {
            attempt.retryRequested = false;
            attempt.cancelWaiting = null;
            reservation = await reserveChatCreation(options, true);
            attemptToken = reservation.attemptToken;
            continue;
          }
          if (error?.definitive && ['chat_create_deleted', 'chat_create_conflict', 'invalid_chat'].includes(error.code)) {
            try { await markChatCreationState(options, error.code === 'chat_create_deleted' ? 'deleted' : 'rejected', attemptToken); }
            catch { /* retain pending */ }
          }
          if (['host_unavailable', 'chat_storage_uncertain', 'invalid_host_response'].includes(error?.code)) {
            error.definitive = false;
          }
          if (error && typeof error === 'object') error.requestId = options.requestId;
          throw error;
        } finally {
          clearTimeout(timer);
          attempt.cancelWaiting = null;
          clearChatCreationPending(attempt);
        }
      }
    })();
    attempt.promise = flight;
    chatCreateFlights.set(options.requestId, attempt);
    try { return await flight; }
    catch (error) {
      if (error && typeof error === 'object') error.requestId = options.requestId;
      throw error;
    }
    finally { if (chatCreateFlights.get(options.requestId) === attempt) chatCreateFlights.delete(options.requestId); }
  }

  async function chatCreationStatus(rawOptions) {
    const options = chatCreationOptions(rawOptions);
    // Failure to read local state must not change the host's read-only query.
    // Without a matching captured attempt no receipt will be changed.
    const observation = await captureChatObservation(options).catch(() => null);
    let summary;
    try { summary = await invoke('agent.chats.creationStatus', options); }
    catch (error) {
      if (error?.definitive && ['chat_create_deleted', 'chat_create_conflict', 'invalid_chat'].includes(error.code)) {
        try { await markChatCreationState(options, error.code === 'chat_create_deleted' ? 'deleted' : 'rejected',
          observation?.attemptToken, observation?.observationToken); }
        catch { /* retain pending */ }
      }
      throw error;
    }
    if (summary === null) return null;
    const handle = requestChatHandle(summary, options.requestId);
    try { await markChatCreationState(options, 'accepted', observation?.attemptToken, observation?.observationToken); }
    catch { /* status remains read-only on the host */ }
    return handle;
  }

  // On a fresh page, reconcile only saved pending identities in this App and
  // revision. This performs bounded, sequential read-only host checks; it
  // never starts a create, selects a Chat, or opens LingXi. Failures leave the
  // receipt pending and an unavailable host is handled silently.
  async function refreshPendingChatCreations() {
    const records = await readChatCreationRecords();
    for (const record of records.slice(0, maxChatCreations)) {
      if (record.state !== 'pending' || chatCreateFlights.has(record.options.requestId)) continue;
      const options = chatCreationOptions(record.options);
      try {
        const observation = await captureChatObservation(options, record.attemptToken, true);
        if (!observation) continue;
        const summary = await invoke('agent.chats.creationStatus', options, {silentRecovery: true});
        if (isRecord(summary) && summary.chatId === options.requestId) {
          // Another tab may have resolved or forgotten the receipt while the
          // read was in flight. Never overwrite that newer state.
          await markChatCreationState(options, 'accepted', observation.attemptToken, observation.observationToken);
        }
      } catch (error) {
        if (error?.code === 'host_unavailable' || error?.code === 'storage_unavailable') return;
        // Any other failure leaves the original receipt unchanged as well.
      }
    }
  }

  const chats = Object.freeze({
    create: createChat,
    creationStatus: chatCreationStatus,
    pendingCreations: pendingChatCreations,
    forgetCreation: forgetChatCreation,
    async open(chatId) { return requestChatHandle(await invoke('agent.chats.get', {chatId})); },
    list(options) { return invoke('agent.chats.list', options || {}); },
    async archive(chatId) { await invoke('agent.chats.archive', {chatId}); },
    async delete(chatId) {
      await invoke('agent.chats.delete', {chatId});
      chatIds.delete(chatId);
      chatMessageIds.delete(chatId);
    }
  });

  const transport = {
    start(input, options = {}) {
      if (options.persistentChat) {
        return startInput('agent.chats.send', {chatId: input.threadId}, input,
          options.knownMessageIds || chatMessageIds.get(input.threadId) || new Set());
      }
      return startInput('agent.tasks.start', {}, input, options.knownMessageIds || new Set());
    },
    events: taskEvents,
    async status({runId}) { return invoke('agent.tasks.get', {runId}); },
    async cancel({runId}) { await invoke('agent.tasks.cancel', {runId}); }
  };
  agent = agui.installBrowserAgent({
    transport,
    tasks,
    chats,
    isPersistentThread: threadId => chatIds.has(threadId)
  });

  async function restoreWakeMarker() {
    const url = new URL(location.href);
    const readyRunIds = url.searchParams.getAll('__lingxi_ready');
    if (!readyRunIds.length) return;
    if (config.hostPlatform !== 'ios' || readyRunIds.length !== 1 || !readyRunIds[0]) {
      consumeWakeMarker();
      return;
    }
    const runId = readyRunIds[0];
    if (restoringWakeRuns.has(runId)) return;
    restoringWakeRuns.add(runId);
    try {
      // Keep the native-return marker in the address until IndexedDB has
      // completed the ownership claim. A transient open/transaction failure
      // can then be retried by the next pageshow or visibilitychange event.
      const outcome = await claimPending(requestKey(runId), runId).catch(error => {
        if (error && typeof error === 'object') error.runId = runId;
        throw error;
      });
      if (outcome.status === 'missing') {
        const error = makeError(
          'The saved Agent request is not available in this browser storage context. Return to the original tab or reopen this handoff in a browser context that shares its storage.',
          'pending_request_missing');
        error.runId = runId;
        handoffWaiters.get(runId)?.reject(error);
        if (!reportedMissingWakeRuns.has(runId)) {
          if (reportedMissingWakeRuns.size >= maxReportedMissingWakeRuns) {
            reportedMissingWakeRuns.delete(reportedMissingWakeRuns.values().next().value);
          }
          reportedMissingWakeRuns.add(runId);
          window.dispatchEvent(new CustomEvent('localApp:agent-handoff-error', {
            detail: {runId, code: error.code, message: error.message}
          }));
        }
        return;
      }
      consumeWakeMarker(runId);
      if (outcome.status === 'accepted') {
        const waiter = handoffWaiters.get(runId);
        const identity = waiter?.identity;
        const receipt = outcome.record;
        if (identity && ['key', 'appId', 'revision', 'runId', 'threadId', 'method', 'fingerprint']
          .every(key => receipt[key] === identity[key])) {
          waiter.resolve({runId});
          window.dispatchEvent(new CustomEvent('localApp:agent-handoff-ready', {detail: {runId}}));
          return;
        }
      }
      if (outcome.status !== 'claimed') {
        const error = makeError(
          'The saved Agent run is not waiting for this handoff. Reopen the app and check its status before choosing an explicit same-ID retry.',
          'pending_request_not_waking');
        error.runId = runId;
        handoffWaiters.get(runId)?.reject(error);
        window.dispatchEvent(new CustomEvent('localApp:agent-handoff-error', {
          detail: {runId, code: error.code, message: error.message}
        }));
        return;
      }
      const record = outcome.record;
      try {
        const accepted = await submitRecord(record);
        handoffWaiters.get(runId)?.resolve(accepted);
        window.dispatchEvent(new CustomEvent('localApp:agent-handoff-ready', {detail: {runId, ...accepted}}));
      } catch (error) {
        handoffWaiters.get(runId)?.reject(error);
        window.dispatchEvent(new CustomEvent('localApp:agent-handoff-error', {
          detail: {runId, code: error.code || 'host_unavailable', message: error.message}
        }));
      }
    } finally {
      restoringWakeRuns.delete(runId);
    }
  }

  function consumeWakeMarker(runId) {
    const currentUrl = new URL(location.href);
    const params = new URLSearchParams();
    let removed = false;
    for (const [name, value] of currentUrl.searchParams) {
      if (name === '__lingxi_ready' && (runId === undefined || value === runId)) {
        removed = true;
        continue;
      }
      params.append(name, value);
    }
    if (!removed) return;
    currentUrl.search = params.toString();
    history.replaceState(history.state, '', `${currentUrl.pathname}${currentUrl.search}${currentUrl.hash}`);
  }

  function serviceUrl(name, path = '/') {
    if (!config.services.includes(name)) throw new Error('Unknown application service.');
    if (typeof path !== 'string' || !path.startsWith('/') || path.startsWith('//') || path.includes('#') || path.includes('\\')) throw new Error('Invalid service path.');
    let decoded;
    try { decoded = decodeURIComponent(path.split('?')[0]); } catch { throw new Error('Invalid service path.'); }
    if (/%2f|%5c/i.test(path.split('?')[0]) || decoded.includes('\\') || decoded.includes('\0') || decoded.split('/').some(part => part === '.' || part === '..')) throw new Error('Invalid service path.');
    return base + '__service/' + encodeURIComponent(name) + path;
  }

  function pickFiles(options = {}) {
    if (!isRecord(options) || Object.keys(options).some(key => !['multiple', 'accept'].includes(key))) {
      throw new TypeError('File picker options may contain only multiple and accept.');
    }
    const multiple = options.multiple === true;
    if (options.multiple !== undefined && typeof options.multiple !== 'boolean') {
      throw new TypeError('multiple must be a boolean.');
    }
    const accept = options.accept;
    const acceptedTypes = accept === undefined ? undefined : Array.isArray(accept) ? accept : [accept];
    if (acceptedTypes && (!acceptedTypes.length || acceptedTypes.length > 32
        || acceptedTypes.some(value => typeof value !== 'string' || !value.trim() || value.length > 128))) {
      throw new TypeError('accept must be a string or a list of nonempty strings.');
    }
    return new Promise((resolve, reject) => {
      const input = document.createElement('input');
      input.type = 'file';
      input.multiple = multiple;
      if (acceptedTypes) input.accept = acceptedTypes.map(value => value.trim()).join(',');
      input.hidden = true;
      document.body.append(input);
      let settled = false;
      const finish = value => { if (settled) return; settled = true; input.remove(); resolve(value); };
      input.addEventListener('cancel', () => finish([]), {once: true});
      input.addEventListener('change', () => finish(Array.from(input.files || [])), {once: true});
      try { input.click(); } catch (error) { input.remove(); reject(error); }
    });
  }

  async function registerRecoveryPage() {
    const detail = {appId: config.appId, revision: config.revision};
    if (!window.isSecureContext || !navigator.serviceWorker || typeof navigator.serviceWorker.register !== 'function') {
      recoveryState.status = 'unavailable';
      recoveryState.reason = 'service_worker_unsupported';
      window.dispatchEvent(new CustomEvent('localApp:recovery-unavailable', {detail: {...detail, reason: recoveryState.reason}}));
      return;
    }
    try {
      const script = new URL(base + '__host/recovery-sw.js', location.href);
      await navigator.serviceWorker.register(script.href, {scope: location.origin + '/'});
      let timer;
      try {
        await Promise.race([
          navigator.serviceWorker.ready,
          new Promise((_, reject) => {
            timer = setTimeout(() => reject(makeError('Offline recovery did not become ready in time.', 'recovery_unavailable')), 15000);
          })
        ]);
      } finally { clearTimeout(timer); }
      recoveryState.status = 'ready';
      delete recoveryState.reason;
      window.dispatchEvent(new CustomEvent('localApp:recovery-ready', {detail}));
    } catch (error) {
      recoveryState.status = 'unavailable';
      recoveryState.reason = error?.code || 'registration_failed';
      window.dispatchEvent(new CustomEvent('localApp:recovery-unavailable', {
        detail: {...detail, reason: recoveryState.reason, message: error?.message || 'Service worker registration failed.'}
      }));
    }
  }

  const files = Object.freeze({pick: pickFiles});
  const host = Object.freeze({
    open: () => { location.href = 'lingxi://apps/' + encodeURIComponent(config.appId) + '?review=agent'; }
  });
  Object.defineProperty(window, 'localApp', {configurable: false, writable: false, value: Object.freeze({
    info: async () => ({
      id: config.appId,
      name: config.name,
      platform: 'mobile-browser',
      hostPlatform: config.hostPlatform,
      revision: config.revision,
      runId: config.runId,
      recovery: {...recoveryState}
    }),
    agent, files, host, serviceUrl,
    serviceSocketUrl: async (name, path = '/') => serviceUrl(name, path).replace(/^http:/, 'ws:')
  })});

  void registerRecoveryPage();
  void refreshPendingChatCreations().catch(() => {});
  void deleteObsoletePendingDatabase();
  const collectExpiredAndNotify = () => collectExpired().then(({expired}) => {
    if (expired) window.dispatchEvent(new CustomEvent('localApp:agent-recovery-expired', {
      detail: {appId: config.appId, count: expired, pendingTtlHours: 24, acceptedTtlDays: 7}
    }));
  }).catch(() => {});
  void collectExpiredAndNotify();
  window.addEventListener('pageshow', () => void collectExpiredAndNotify());
  setInterval(() => {
    if (document.visibilityState === 'visible') void collectExpiredAndNotify();
  }, 60 * 60 * 1000);

  const restoreSafely = () => {
    void restoreWakeMarker().catch(error => {
      window.dispatchEvent(new CustomEvent('localApp:agent-handoff-error', {
        detail: {runId: error.runId, code: error.code || 'storage_unavailable', message: error.message}
      }));
    });
  };
  window.addEventListener('pageshow', restoreSafely);
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') restoreSafely();
  });
  restoreSafely();
})();
