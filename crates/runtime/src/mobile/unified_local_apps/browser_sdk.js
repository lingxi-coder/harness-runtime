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
  const pendingStore = 'pending';
  const pendingDbName = 'lingxi-local-app-agui-' + config.appId;
  const obsoletePendingDbName = 'lingxi-local-app-agent-' + config.appId;
  const pendingPrefix = config.appId + ':' + config.revision + ':';
  const inFlight = new Map();
  const handoffWaiters = new Map();
  const restoringWakeRuns = new Set();
  const chatIds = new Set();
  const chatMessageIds = new Map();
  const recoveryState = {status: 'checking'};
  const textEncoder = new TextEncoder();

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
      let expired = 0;
      cursorRequest.onsuccess = () => {
        const cursor = cursorRequest.result;
        if (!cursor) return;
        const record = cursor.value;
        if (record?.appId === config.appId && recordExpired(record)) {
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
      let claimed = null;
      request.onsuccess = () => {
        const record = request.result;
        if (record && recordExpired(record)) {
          store.delete(key);
          return;
        }
        if (record && record.runId === runId && record.state === 'waking'
            && record.appId === config.appId && record.revision === config.revision) {
          record.state = 'sending';
          record.sendingAt = Date.now();
          record.storageBytes = recordBytes(record);
          store.put(record);
          claimed = record;
        }
      };
      tx.oncomplete = () => { db.close(); resolve(claimed); };
      tx.onerror = () => { db.close(); reject(makeError('Could not restore the saved Agent run.', 'storage_unavailable', tx.error)); };
      tx.onabort = () => { db.close(); reject(makeError('Could not restore the saved Agent run.', 'storage_unavailable', tx.error)); };
    });
  }

  async function invoke(method, args = {}) {
    const body = JSON.stringify({method, args});
    if (utf8ByteLengthInChunks(body) > maxRequestBodyBytes) {
      throw makeError('The AG-UI request exceeds the 116 MiB transport limit.', 'invalid_input');
    }
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 120000);
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
      throw makeError('LingXi is unavailable or paused. The run is saved for an explicit retry.', 'host_unavailable', cause);
    } finally {
      clearTimeout(timer);
    }
    if (!response.ok) {
      const error = makeError('The application run is unavailable. Return to LingXi to resume.', 'host_unavailable');
      error.definitive = true;
      throw error;
    }
    let result;
    try { result = await response.json(); }
    catch (cause) { throw makeError('The host returned an invalid Agent response.', 'invalid_host_response', cause); }
    if (!result?.ok) {
      const detail = result?.error || {};
      const error = makeError(detail.message || 'Agent request failed.', detail.code || 'agent_error');
      error.definitive = true;
      throw error;
    }
    return result.value;
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

  function waitForHandoff(runId) {
    let waiter;
    const promise = new Promise((resolve, reject) => {
      const timeout = setTimeout(() => {
        handoffWaiters.delete(runId);
        reject(makeError('Return to this app after LingXi confirms the Agent handoff.', 'handoff_timeout'));
      }, 120000);
      waiter = {
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
      const pending = waitForHandoff(normalized.runId);
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
    const poll = async () => {
      try {
        const page = await taskEvents(afterSequence === undefined ? {runId} : {runId, afterSequence});
        if (!active) return;
        for (const event of page.events) {
          listener(event);
          if (event.type === 'RUN_FINISHED' || event.type === 'RUN_ERROR') {
            active = false;
            clearTimeout(timer);
            return;
          }
        }
        afterSequence = page.sequence;
        const task = await invoke('agent.tasks.get', {runId});
        if (['succeeded', 'failed', 'cancelled', 'interrupted'].includes(task?.state)) {
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

  function requestChatHandle(summary) {
    const id = summary?.chatId;
    if (typeof id !== 'string' || !id) throw makeError('The host returned an invalid Chat ID.', 'invalid_host_response');
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
  const chats = Object.freeze({
    async create(options = {}) { return requestChatHandle(await invoke('agent.chats.create', options)); },
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
      const record = await claimPending(requestKey(runId), runId);
      consumeWakeMarker(runId);
      if (!record) return;
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
