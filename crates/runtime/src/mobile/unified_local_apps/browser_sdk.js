/* Served by the run-scoped LingXi loopback gateway. No native message bridge. */
(() => {
  if (window.top !== window || window.localApp) return;
  const config = __LINGXI_APP_CONFIGURATION__;
  const base = `${location.origin}/__run/${encodeURIComponent(config.runId)}/`;
  const maxFiles = 8;
  const maxFileBytes = 25 * 1024 * 1024;
  const maxTotalBytes = 50 * 1024 * 1024;
  const pendingTtlMs = 24 * 60 * 60 * 1000;
  const acceptedTtlMs = 7 * 24 * 60 * 60 * 1000;
  const maxPendingRecords = 32;
  const maxPendingStorageBytes = 100 * 1024 * 1024;
  const pendingStore = 'pending';
  const pendingDbName = `lingxi-local-app-agent-${config.appId}`;
  const pendingPrefix = `${config.appId}:${config.revision}:`;
  const inFlight = new Map();
  const handoffWaiters = new Map();
  const recoveryState = {status: 'checking'};

  const makeError = (message, code = 'agent_error', cause) => {
    const error = new Error(message, cause ? {cause} : undefined);
    error.code = code;
    return error;
  };

  function requestId() {
    if (typeof crypto.randomUUID === 'function') return crypto.randomUUID();
    const bytes = new Uint8Array(16);
    crypto.getRandomValues(bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    return [...bytes].map((value, index) => `${index === 4 || index === 6 || index === 8 || index === 10 ? '-' : ''}${value.toString(16).padStart(2, '0')}`).join('');
  }

  function isRecord(value) {
    if (!value || typeof value !== 'object' || Array.isArray(value)) return false;
    const prototype = Object.getPrototypeOf(value);
    return prototype === Object.prototype || prototype === null;
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

  function recordExpired(record, now = Date.now()) {
    const accepted = record?.state === 'accepted';
    const timestamp = accepted ? record.acceptedAt : record?.createdAt;
    const ttl = accepted ? acceptedTtlMs : pendingTtlMs;
    return !Number.isFinite(timestamp) || timestamp > now + 5 * 60 * 1000 || now - timestamp >= ttl;
  }

  function recordBytes(record) {
    let metadata;
    try {
      metadata = JSON.stringify({key: record.key, appId: record.appId, revision: record.revision,
        requestId: record.requestId, method: record.method, args: record.args,
        fingerprint: record.fingerprint, state: record.state, runId: record.runId});
    } catch { return maxPendingStorageBytes + 1; }
    const files = Array.isArray(record.files) ? record.files : [];
    return new TextEncoder().encode(metadata || '').byteLength + files.reduce((total, file) => total + (file?.size || 0), 0);
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
      request.onsuccess = () => {
        const existing = request.result;
        if (existing && (existing.appId !== config.appId || existing.revision !== config.revision
            || existing.requestId !== record.requestId || existing.fingerprint !== record.fingerprint
            || existing.method !== record.method)) {
          failure = makeError('This requestId is already bound to different input.', 'request_id_conflict');
          return;
        }
        if (existing?.state === 'accepted' && record.state !== 'accepted') return;
        if (!existing && record.state !== 'accepted') {
          failure = makeError('The saved Agent request expired; create a new request after reviewing it.', 'request_expired');
          return;
        }
        if (existing && recordExpired(existing) && record.state !== 'accepted') {
          store.delete(record.key);
          failure = makeError('The saved Agent request expired; create a new request after reviewing it.', 'request_expired');
          return;
        }
        store.put(record);
      };
      tx.oncomplete = () => { db.close(); failure ? reject(failure) : resolve(); };
      tx.onerror = () => { db.close(); reject(makeError('Could not save the Agent request for recovery.', 'storage_unavailable', tx.error)); };
      tx.onabort = () => { db.close(); reject(makeError('Could not save the Agent request for recovery.', 'storage_unavailable', tx.error)); };
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
            conflict = existing.appId !== config.appId || existing.revision !== config.revision
              || existing.requestId !== record.requestId || existing.fingerprint !== record.fingerprint
              || existing.method !== record.method;
            if (conflict) return;
            if (existing.state === 'accepted') { result = existing; return; }
            if (existing.state === 'rejected') {
              const projectedBytes = liveBytes - recordBytes(existing) + recordBytes(record);
              if (projectedBytes > maxPendingStorageBytes) {
                failure = makeError('Browser recovery storage is full. Forget an old request and try again.', 'storage_full');
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
            failure = makeError('Browser recovery storage is full. Forget an old request and try again.', 'storage_full');
            return;
          }
          store.add(record);
          result = record;
        };
      };
      tx.oncomplete = () => {
        db.close();
        if (conflict) reject(makeError('This requestId is already bound to different input.', 'request_id_conflict'));
        else if (failure) reject(failure);
        else resolve(result);
      };
      tx.onerror = () => { db.close(); reject(makeError('Could not save the Agent request for recovery.', 'storage_unavailable', tx.error)); };
      tx.onabort = () => { db.close(); reject(makeError('Could not save the Agent request for recovery.', 'storage_unavailable', tx.error)); };
    });
  }

  async function claimPending(key, requestIdValue) {
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
        if (record && record.requestId === requestIdValue && record.state === 'waking'
            && record.appId === config.appId && record.revision === config.revision) {
          record.state = 'sending';
          record.sendingAt = Date.now();
          store.put(record);
          claimed = record;
        }
      };
      tx.oncomplete = () => { db.close(); resolve(claimed); };
      tx.onerror = () => { db.close(); reject(makeError('Could not restore the saved Agent request.', 'storage_unavailable', tx.error)); };
      tx.onabort = () => { db.close(); reject(makeError('Could not restore the saved Agent request.', 'storage_unavailable', tx.error)); };
    });
  }

  function validFile(file) {
    return typeof File === 'function' && file instanceof File;
  }

  function safeFileName(name) {
    return typeof name === 'string' && name.length > 0 && name.length <= 255
      && name !== '.' && name !== '..' && !/[\/\\\u0000-\u001f\u007f]/u.test(name);
  }

  function canonical(value) {
    if (Array.isArray(value)) return value.map(canonical);
    if (isRecord(value)) {
      return Object.fromEntries(Object.keys(value).sort().map(key => [key, canonical(value[key])]));
    }
    return value;
  }

  async function fingerprint(metadata, files) {
    if (!crypto.subtle) throw makeError('Secure file recovery is unavailable in this browser.', 'storage_unavailable');
    const fileHashes = [];
    for (const file of files) {
      const digest = await crypto.subtle.digest('SHA-256', await file.arrayBuffer());
      fileHashes.push({name: file.name, mediaType: file.type || 'application/octet-stream', size: file.size,
        digest: [...new Uint8Array(digest)].map(value => value.toString(16).padStart(2, '0')).join('')});
    }
    const bytes = new TextEncoder().encode(JSON.stringify(canonical({metadata, files: fileHashes})));
    const digest = await crypto.subtle.digest('SHA-256', bytes);
    return [...new Uint8Array(digest)].map(value => value.toString(16).padStart(2, '0')).join('');
  }

  async function normalizeInput(input) {
    if (!isRecord(input)) throw new TypeError('Agent input must be an object.');
    const allowed = new Set(['requestId', 'prompt', 'attachments', 'outputSchema', 'tools', 'skills', 'maxTurns', 'timeoutMs']);
    for (const key of Object.keys(input)) {
      if (!allowed.has(key)) throw new TypeError(`Unsupported Agent input field: ${key}`);
    }
    const id = input.requestId === undefined ? requestId() : input.requestId;
    if (typeof id !== 'string' || id.length < 1 || id.length > 128 || /[\u0000-\u001f\u007f]/u.test(id)) {
      throw new TypeError('requestId must be a nonempty string of at most 128 characters.');
    }
    if (typeof input.prompt !== 'string') throw new TypeError('prompt must be a string.');
    const attachments = input.attachments === undefined ? [] : input.attachments;
    if (!Array.isArray(attachments) || attachments.length > maxFiles || attachments.some(file => !validFile(file))) {
      throw new TypeError('attachments must contain up to eight browser File objects.');
    }
    let total = 0;
    for (const file of attachments) {
      if (!safeFileName(file.name)) throw new TypeError('Each attachment needs a safe file name.');
      if (file.size > maxFileBytes) throw makeError('Each attachment must be 25 MiB or smaller.', 'attachment_too_large');
      total += file.size;
      if (total > maxTotalBytes) throw makeError('Attachments must total 50 MiB or less.', 'attachments_too_large');
    }
    const metadata = {};
    for (const key of ['prompt', 'outputSchema', 'tools', 'skills', 'maxTurns', 'timeoutMs']) {
      if (input[key] !== undefined) metadata[key] = input[key];
    }
    metadata.requestId = id;
    let serialized;
    try { serialized = JSON.stringify(metadata); }
    catch (cause) { throw makeError('Agent input must contain JSON-compatible options.', 'invalid_input', cause); }
    if (typeof serialized !== 'string' || new TextEncoder().encode(serialized).byteLength > 1024 * 1024) {
      throw makeError('Agent metadata must be 1 MiB or smaller.', 'invalid_input');
    }
    return {id, metadata, files: [...attachments]};
  }

  function requestKey(id) { return `${pendingPrefix}${id}`; }

  async function storeRequest(method, args, normalized) {
    const key = requestKey(normalized.id);
    const metadata = {method, args};
    const digest = await fingerprint(metadata, normalized.files);
    const record = {key, appId: config.appId, revision: config.revision, requestId: normalized.id,
      method, args, files: normalized.files, fingerprint: digest, state: 'pending', createdAt: Date.now()};
    return reservePending(record);
  }

  async function finishAccepted(record, runId) {
    await writePending({key: record.key, appId: config.appId, revision: config.revision,
      requestId: record.requestId, method: record.method, fingerprint: record.fingerprint,
      state: 'accepted', runId, acceptedAt: Date.now()});
    return {runId};
  }

  function hostError(result) {
    const detail = result?.error || {};
    const error = makeError(detail.message || 'Agent request failed.', detail.code || 'agent_error');
    // A host error envelope means admission failed before a runId was created.
    error.definitive = true;
    return error;
  }

  async function invoke(method, args = {}, files = []) {
    const form = new FormData();
    form.append('request', JSON.stringify({method, args}));
    for (const file of files) form.append('attachment', file, file.name);
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 120000);
    let response;
    try {
      response = await fetch(`${base}__host/agent`, {
        method: 'POST', credentials: 'omit', cache: 'no-store', redirect: 'error',
        headers: {'X-LingXi-App-Request': '1'}, body: form, signal: controller.signal
      });
    } catch (cause) {
      const error = makeError('LingXi is unavailable or paused. The request is saved for an explicit retry.', 'host_unavailable', cause);
      throw error;
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
    if (!result?.ok) throw hostError(result);
    return result.value;
  }

  function wakeLink(requestIdValue) {
    const url = new URL(`lingxi://apps/${encodeURIComponent(config.appId)}`);
    url.searchParams.set('review', 'agent');
    url.searchParams.set('requestId', requestIdValue);
    url.searchParams.set('revision', String(config.revision));
    return url.href;
  }

  function waitForHandoff(requestIdValue) {
    let waiter;
    const promise = new Promise((resolve, reject) => {
      const timeout = setTimeout(() => {
        handoffWaiters.delete(requestIdValue);
        reject(makeError('Return to this app after LingXi confirms the Agent handoff.', 'handoff_timeout'));
      }, 120000);
      waiter = {
        promise: null,
        resolve(value) { clearTimeout(timeout); handoffWaiters.delete(requestIdValue); resolve(value); },
        reject(error) { clearTimeout(timeout); handoffWaiters.delete(requestIdValue); reject(error); }
      };
      handoffWaiters.set(requestIdValue, waiter);
    });
    waiter.promise = promise;
    return promise;
  }

  async function submitRecord(record) {
    const key = record.key;
    if (record.state === 'accepted' && record.runId) return {runId: record.runId};
    if (inFlight.has(key)) return inFlight.get(key);
    const promise = (async () => {
      const active = {...record, state: 'sending', sendingAt: Date.now()};
      await writePending(active);
      try {
        const value = await invoke(record.method, record.args, record.files || []);
        if (!value || typeof value.runId !== 'string' || !value.runId) {
          throw makeError('The host did not return a task ID.', 'invalid_host_response');
        }
        return await finishAccepted(record, value.runId);
      } catch (error) {
        if (error.definitive) {
          // Keep only a short dedup receipt. The user can explicitly retry the
          // same requestId and reattach the same input; this record retains no
          // prompt or file bytes after the host rejects admission.
          await writePending({key: record.key, appId: record.appId, revision: record.revision,
            requestId: record.requestId, method: record.method, fingerprint: record.fingerprint,
            state: 'rejected', createdAt: record.createdAt, lastError: error.code || 'agent_rejected'});
        } else {
          await writePending({...record, state: 'uncertain', lastError: error.code || 'host_unavailable', lastAttemptAt: Date.now()});
        }
        throw error;
      }
    })();
    inFlight.set(key, promise);
    try { return await promise; }
    finally { inFlight.delete(key); }
  }

  async function startInput(method, extraArgs, input) {
    const normalized = await normalizeInput(input);
    const args = {...extraArgs, input: normalized.metadata};
    const record = await storeRequest(method, args, normalized);
    if (record.state === 'accepted' && record.runId) return {runId: record.runId};
    if (inFlight.has(record.key)) return inFlight.get(record.key);
    if (config.hostPlatform === 'ios') {
      if (record.state !== 'waking') {
        await writePending({...record, state: 'waking', wakeRequestedAt: Date.now()});
      }
      const currentWaiter = handoffWaiters.get(normalized.id);
      if (currentWaiter) return currentWaiter.promise;
      const pending = waitForHandoff(normalized.id);
      try { location.assign(wakeLink(normalized.id)); }
      catch (cause) {
        handoffWaiters.get(normalized.id)?.reject(makeError('Could not open LingXi for the Agent handoff.', 'handoff_unavailable', cause));
      }
      return pending;
    }
    return submitRecord(record);
  }

  function terminal(task) {
    return ['succeeded', 'failed', 'cancelled', 'interrupted'].includes(task?.state);
  }

  function taskReply(task) {
    const result = task?.result;
    const usage = task?.usage;
    if (!task || typeof task.runId !== 'string' || !task.runId || !result || typeof result.text !== 'string'
        || !isRecord(usage)
        || Object.keys(usage).some(key => !['inputTokens', 'outputTokens', 'costUsd', 'costEstimated', 'turns'].includes(key))
        || (usage.inputTokens !== undefined && (!Number.isSafeInteger(usage.inputTokens) || usage.inputTokens < 0))
        || (usage.outputTokens !== undefined && (!Number.isSafeInteger(usage.outputTokens) || usage.outputTokens < 0))
        || (usage.costUsd !== undefined && (typeof usage.costUsd !== 'number' || !Number.isFinite(usage.costUsd) || usage.costUsd < 0))
        || (usage.costEstimated !== undefined && typeof usage.costEstimated !== 'boolean')
        || (usage.turns !== undefined && (!Number.isSafeInteger(usage.turns) || usage.turns < 0))
        || !Array.isArray(result.attachments) || result.attachments.length !== 0) {
      throw makeError('The completed task is missing its result or has invalid usage details.', 'invalid_host_response');
    }
    return {runId: task.runId, text: typeof result.text === 'string' ? result.text : '',
      ...(result.output === undefined ? {} : {output: result.output}),
      ...(result.chatId === undefined ? {} : {chatId: result.chatId}),
      ...(result.messageId === undefined ? {} : {messageId: result.messageId}),
      attachments: [],
      usage};
  }

  function taskFailure(task) {
    const error = makeError(task?.error?.message || `Agent task ended in ${task?.state || 'an unknown state'}.`,
      task?.error?.code || task?.state || 'agent_task_failed');
    error.task = task;
    return error;
  }

  function subscribeTask(runId, listener) {
    if (typeof listener !== 'function') throw new TypeError('A listener is required.');
    let active = true;
    let timer = null;
    let sequence = -1;
    const poll = async () => {
      try {
        const task = await invoke('agent.tasks.get', {runId});
        if (!active) return;
        if (Number.isSafeInteger(task?.sequence) && task.sequence > sequence) {
          sequence = task.sequence;
          listener({type: 'state', runId, sequence, time: task.updatedAt, task});
        }
        if (terminal(task)) { active = false; return; }
      } catch (error) {
        if (active) listener({type: 'connection', runId, error: {code: error.code || 'host_unavailable', message: error.message}});
      }
      if (active) timer = setTimeout(poll, 1000);
    };
    void poll();
    return () => { active = false; clearTimeout(timer); };
  }

  function waitForTaskRecord(runId) {
    return new Promise((resolve, reject) => {
      let unsubscribe = () => {};
      unsubscribe = subscribeTask(runId, event => {
        if (event.type === 'connection') {
          unsubscribe();
          reject(makeError(event.error?.message || 'The application host is unavailable.', event.error?.code || 'host_unavailable'));
          return;
        }
        if (event.type !== 'state' || !terminal(event.task)) return;
        unsubscribe();
        if (event.task.state === 'succeeded') resolve(event.task);
        else reject(taskFailure(event.task));
      });
    });
  }

  function waitForTask(runId) { return waitForTaskRecord(runId).then(taskReply); }

  function chatHandle(summary) {
    const id = summary.chatId;
    if (typeof id !== 'string' || !id) throw makeError('The host returned an invalid Chat ID.', 'invalid_host_response');
    const handle = {
      id,
      async send(input) {
        const {runId} = await startInput('agent.chats.send', {chatId: id}, input);
        const task = await waitForTaskRecord(runId);
        const reply = taskReply(task);
        if (reply.chatId !== id || typeof reply.messageId !== 'string' || !reply.messageId) {
          throw makeError('The completed chat task is missing its durable message identity.', 'invalid_host_response');
        }
        return reply;
      },
      history(options = {}) { return invoke('agent.chats.history', {...options, chatId: id}); },
      subscribe(listener) {
        if (typeof listener !== 'function') throw new TypeError('A listener is required.');
        let active = true;
        let timer;
        let summaryKey = '';
        let seen = new Set();
        const poll = async () => {
          try {
            const current = await invoke('agent.chats.get', {chatId: id});
            if (!active) return;
            const key = `${current.updatedAt}:${current.state}`;
            if (key !== summaryKey) {
              summaryKey = key;
              listener({type: 'state', chatId: id, chat: current});
            }
            const page = await invoke('agent.chats.history', {chatId: id, limit: 50});
            const messages = Array.isArray(page?.messages) ? page.messages : [];
            const added = messages.filter(message => message.messageId && !seen.has(message.messageId));
            for (const message of messages) if (message.messageId) seen.add(message.messageId);
            if (added.length) listener({type: 'messages', chatId: id, messages: added, nextCursor: page.nextCursor});
          } catch (error) {
            if (active) listener({type: 'connection', chatId: id, error: {code: error.code || 'host_unavailable', message: error.message}});
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

  async function restoreWakeMarker() {
    const url = new URL(location.href);
    const readyIds = url.searchParams.getAll('__lingxi_ready');
    if (!readyIds.length) return;
    url.searchParams.delete('__lingxi_ready');
    history.replaceState(history.state, '', `${url.pathname}${url.search}${url.hash}`);
    if (config.hostPlatform !== 'ios' || readyIds.length !== 1 || !readyIds[0]) return;
    const id = readyIds[0];
    const key = requestKey(id);
    const record = await claimPending(key, id);
    if (!record) return;
    const current = {...record, state: 'sending'};
    try {
      const accepted = await submitRecord(current);
      handoffWaiters.get(id)?.resolve(accepted);
    } catch (error) {
      handoffWaiters.get(id)?.reject(error);
      window.dispatchEvent(new CustomEvent('localApp:agent-handoff-error', {detail: {requestId: id, code: error.code, message: error.message}}));
    }
  }

  function serviceUrl(name, path = '/') {
    if (!config.services.includes(name)) throw new Error('Unknown application service.');
    if (typeof path !== 'string' || !path.startsWith('/') || path.startsWith('//') || path.includes('#') || path.includes('\\')) throw new Error('Invalid service path.');
    let decoded;
    try { decoded = decodeURIComponent(path.split('?')[0]); } catch { throw new Error('Invalid service path.'); }
    if (/%2f|%5c/i.test(path.split('?')[0]) || decoded.includes('\\') || decoded.includes('\0') || decoded.split('/').some(part => part === '.' || part === '..')) throw new Error('Invalid service path.');
    return `${base}__service/${encodeURIComponent(name)}${path}`;
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
      const script = new URL(`${base}__host/recovery-sw.js`, location.href);
      await navigator.serviceWorker.register(script.href, {scope: `${location.origin}/`});
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

  const tasks = Object.freeze({
    start: input => startInput('agent.tasks.start', {}, input),
    get: runId => invoke('agent.tasks.get', {runId}),
    list: options => invoke('agent.tasks.list', options || {}),
    subscribe: subscribeTask,
    async cancel(runId) { await invoke('agent.tasks.cancel', {runId}); }
  });
  const chats = Object.freeze({
    async create(options = {}) { return chatHandle(await invoke('agent.chats.create', options)); },
    async open(chatId) { return chatHandle(await invoke('agent.chats.get', {chatId})); },
    list: options => invoke('agent.chats.list', options || {}),
    async archive(chatId) { await invoke('agent.chats.archive', {chatId}); },
    async delete(chatId) { await invoke('agent.chats.delete', {chatId}); }
  });
  const agent = Object.freeze({
    async run(input) { const {runId} = await tasks.start(input); return waitForTask(runId); },
    tasks,
    chats
  });
  const host = Object.freeze({
    open: () => { location.href = `lingxi://apps/${encodeURIComponent(config.appId)}?review=agent`; }
  });
  const files = Object.freeze({pick: pickFiles});
  Object.defineProperty(window, 'localApp', {configurable: false, writable: false, value: Object.freeze({
    info: async () => ({id: config.appId, name: config.name, platform: 'mobile-browser', hostPlatform: config.hostPlatform, revision: config.revision, runId: config.runId, recovery: {...recoveryState}}),
    agent, files, host, serviceUrl,
    serviceSocketUrl: async (name, path = '/') => serviceUrl(name, path).replace(/^http:/, 'ws:')
  })});
  void registerRecoveryPage();
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
      window.dispatchEvent(new CustomEvent('localApp:agent-handoff-error', {detail: {code: error.code || 'storage_unavailable', message: error.message}}));
    });
  };
  window.addEventListener('pageshow', restoreSafely);
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') restoreSafely();
  });
  restoreSafely();
})();
