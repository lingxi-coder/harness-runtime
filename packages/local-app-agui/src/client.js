import {AbstractAgent} from '@ag-ui/client';
import {EMPTY, Observable} from 'rxjs';
import {preflightJson} from './preflight.js';
import {validateEvent, validateRunAgentInput} from './validators.js';

export const LOCAL_APP_EXTENSION_KEY = 'com.lingxi.localApp';
export const AG_UI_PROTOCOL_VERSION = '1.0';
const MAX_EVENT_PAGE_EVENTS = 128;
const MAX_RUN_EVENTS = 10_000;
const MAX_TERMINAL_DRAIN_PAGES = 10_000;

function isRecord(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return false;
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}

function isEmptyState(value) {
  return value === undefined || value === null
    || (Array.isArray(value) && value.length === 0)
    || (isRecord(value) && Object.keys(value).length === 0);
}

function base64Value(code) {
  if (code >= 65 && code <= 90) return code - 65;
  if (code >= 97 && code <= 122) return code - 71;
  if (code >= 48 && code <= 57) return code + 4;
  if (code === 43) return 62;
  if (code === 47) return 63;
  return -1;
}

function base64ByteLength(value) {
  if (typeof value !== 'string' || value.length % 4 !== 0) return null;
  const length = value.length;
  let padding = 0;
  if (length && value.charCodeAt(length - 1) === 61) {
    padding = 1;
    if (length > 1 && value.charCodeAt(length - 2) === 61) padding = 2;
  }
  const bodyLength = length - padding;
  if ((padding === 1 && bodyLength % 4 !== 3)
      || (padding === 2 && bodyLength % 4 !== 2)
      || (!padding && bodyLength % 4 !== 0)) return null;
  for (let index = 0; index < bodyLength; index++) {
    if (base64Value(value.charCodeAt(index)) < 0) return null;
  }
  for (let index = bodyLength; index < length; index++) {
    if (value.charCodeAt(index) !== 61) return null;
  }
  if (padding === 2 && (base64Value(value.charCodeAt(length - 3)) & 0x0f) !== 0) return null;
  if (padding === 1 && (base64Value(value.charCodeAt(length - 2)) & 0x03) !== 0) return null;
  return (length / 4) * 3 - padding;
}

function localAppInput(value, {persistentChat = false, knownMessageIds = new Set()} = {}) {
  const input = validateRunAgentInput(value);
  if (input.protocolVersion !== undefined && input.protocolVersion !== AG_UI_PROTOCOL_VERSION) {
    throw new TypeError(`Only AG-UI ${AG_UI_PROTOCOL_VERSION} RunAgentInput is supported.`);
  }
  input.protocolVersion ??= AG_UI_PROTOCOL_VERSION;
  if (input.tools.length) throw new TypeError('Frontend tool definitions are not supported by this Local App host.');
  if (input.context.length) throw new TypeError('RunAgentInput.context is not supported by this Local App host.');
  if (!isEmptyState(input.state)) throw new TypeError('RunAgentInput.state is not supported by this Local App host.');
  if (input.resume?.length) throw new TypeError('RunAgentInput.resume is not supported by this Local App host.');

  const forwardedProps = input.forwardedProps === undefined ? {} : input.forwardedProps;
  if (!isRecord(forwardedProps)) throw new TypeError('forwardedProps must be an object for Local App options.');
  if (Object.keys(forwardedProps).some(key => key !== LOCAL_APP_EXTENSION_KEY)) {
    throw new TypeError('This Local App host supports only forwardedProps.com.lingxi.localApp.');
  }
  const extension = forwardedProps[LOCAL_APP_EXTENSION_KEY];
  if (extension === undefined) {
    forwardedProps[LOCAL_APP_EXTENSION_KEY] = {version: 1};
  } else if (!isRecord(extension) || extension.version !== 1) {
    throw new TypeError(`forwardedProps.${LOCAL_APP_EXTENSION_KEY} must use version 1.`);
  } else {
    const allowed = new Set(['version', 'outputSchema', 'tools', 'skills', 'maxTurns', 'timeoutMs']);
    if (Object.keys(extension).some(key => !allowed.has(key))) {
      throw new TypeError(`forwardedProps.${LOCAL_APP_EXTENSION_KEY} contains an unsupported field.`);
    }
    if (extension.outputSchema !== undefined) {
      let serialized;
      try { serialized = preflightJson(extension.outputSchema, {maxBytes: 256 * 1024, maxDepth: 64, maxNodes: 100_000, maxArrayItems: 10_000, maxObjectProperties: 10_000, maxStringBytes: 256 * 1024}); }
      catch { throw new TypeError('The Local App outputSchema must be JSON-compatible and no larger than 256 KiB.'); }
      if (!isRecord(extension.outputSchema) || serialized.bytes > 256 * 1024) {
        throw new TypeError('The Local App outputSchema must be an object no larger than 256 KiB.');
      }
    }
    for (const field of ['tools', 'skills']) {
      const ids = extension[field];
      if (ids === undefined) continue;
      if (!Array.isArray(ids) || ids.length > 128
          || ids.some(id => typeof id !== 'string' || id.length === 0 || id.length > 128 || /[\u0000-\u001f\u007f]/u.test(id))
          || new Set(ids).size !== ids.length) {
        throw new TypeError(`Local App ${field} must contain up to 128 unique IDs.`);
      }
      if (field === 'skills' && ids.some(id => !/^[A-Za-z0-9._-]+$/u.test(id))) {
        throw new TypeError('Local App skills must use valid Skill IDs.');
      }
    }
    if (extension.maxTurns !== undefined && (!Number.isSafeInteger(extension.maxTurns)
        || extension.maxTurns < 1 || extension.maxTurns > 100)) {
      throw new RangeError('Local App maxTurns must be from 1 to 100.');
    }
    if (extension.timeoutMs !== undefined && (!Number.isSafeInteger(extension.timeoutMs)
        || extension.timeoutMs < 1 || extension.timeoutMs > 3600000)) {
      throw new RangeError('Local App timeoutMs must be from 1 to 3600000.');
    }
  }
  input.forwardedProps = forwardedProps;

  if (!persistentChat && (input.messages.length !== 1 || input.messages[0]?.role !== 'user')) {
    throw new TypeError('An independent Local App run requires exactly one user message on a fresh thread.');
  }
  if (persistentChat) {
    const newUserMessages = input.messages.filter(message => message.role === 'user' && !knownMessageIds.has(message.id));
    if (newUserMessages.length !== 1 || input.messages.at(-1)?.id !== newUserMessages[0]?.id) {
      throw new TypeError('A persistent Local App chat run requires one new user message after its canonical history.');
    }
  }
  let newFileCount = 0;
  let newFileBytes = 0;
  for (const message of input.messages) {
    if (typeof message.content === 'string' || !Array.isArray(message.content)) continue;
    for (const part of message.content) {
      if (part.type === 'text') continue;
      if (part.type !== 'image' && part.type !== 'document') {
        throw new TypeError(`AG-UI ${part.type} parts are not supported by this Local App host.`);
      }
      if (part.source.type !== 'data') {
        throw new TypeError('Only inline AG-UI image and document DataSource parts are supported.');
      }
      const encoded = part.source.value;
      const byteLength = base64ByteLength(encoded);
      if (byteLength === null) {
        throw new TypeError('An inline AG-UI DataSource must contain valid base64.');
      }
      if (knownMessageIds.has(message.id)) continue;
      if (byteLength > 25 * 1024 * 1024) throw new RangeError('Each attachment must be 25 MiB or smaller.');
      newFileCount++;
      newFileBytes += byteLength;
      if (newFileCount > 8 || newFileBytes > 50 * 1024 * 1024) {
        throw new RangeError('A Local App run supports up to eight files and 50 MiB total.');
      }
    }
  }
  return input;
}

export function validateLocalAppRunInput(value, options) {
  return localAppInput(value, options);
}

function delay(ms, signal, setTimer) {
  return new Promise(resolve => {
    if (signal.aborted) return resolve();
    const timer = setTimer(() => {
      signal.removeEventListener('abort', onAbort);
      resolve();
    }, ms);
    const onAbort = () => {
      clearTimeout(timer);
      resolve();
    };
    signal.addEventListener('abort', onAbort, {once: true});
  });
}

function validateEventPage(page, previousSequence) {
  preflightJson(page, {maxBytes: 8 * 1024 * 1024, maxDepth: 64, maxNodes: 100_000, maxArrayItems: 10_000, maxObjectProperties: 10_000, maxStringBytes: 8 * 1024 * 1024});
  if (!isRecord(page) || !Array.isArray(page.events) || !Number.isSafeInteger(page.sequence) || page.sequence < 0) {
    throw new TypeError('The host returned an invalid AG-UI event page.');
  }
  if (page.events.length > MAX_EVENT_PAGE_EVENTS) {
    throw new TypeError('The host returned an AG-UI event page with more than ' + MAX_EVENT_PAGE_EVENTS + ' events.');
  }
  if (previousSequence !== undefined && page.sequence < previousSequence) {
    throw new TypeError('The host returned a regressing AG-UI event cursor.');
  }
  if (previousSequence !== undefined && page.events.length > 0 && page.sequence === previousSequence) {
    throw new TypeError('The host returned events without advancing the AG-UI event cursor.');
  }
  return page.events.map(event => validateEvent(event));
}

function terminalTask(task) {
  return ['succeeded', 'failed', 'cancelled', 'interrupted'].includes(task?.state);
}

function eventStream({input, transport, start, knownMessageIds, persistentChat, pollIntervalMs, onAccepted}) {
  return new Observable(subscriber => {
    const controller = new AbortController();
    let active = true;

    const poll = async () => {
      let afterSequence;
      let observedEvents = 0;
      let terminalStatusObserved = false;
      let terminalDrainPages = 0;
      try {
        if (start) {
          const accepted = await transport.start(input, {knownMessageIds, persistentChat});
          if (!accepted || accepted.runId !== input.runId) {
            throw new TypeError('The host did not preserve the AG-UI runId.');
          }
          onAccepted?.();
        }
        while (active && !controller.signal.aborted) {
          if (terminalStatusObserved && terminalDrainPages >= MAX_TERMINAL_DRAIN_PAGES) {
            throw new TypeError('The host returned too many AG-UI event pages without a terminal event.');
          }
          const request = {runId: input.runId};
          if (afterSequence !== undefined) request.afterSequence = afterSequence;
          const page = await transport.events(request, {signal: controller.signal});
          if (!active || controller.signal.aborted) return;
          if (terminalStatusObserved) terminalDrainPages++;
          const events = validateEventPage(page, afterSequence);
          observedEvents += events.length;
          if (observedEvents > MAX_RUN_EVENTS) {
            throw new TypeError('The host returned more than ' + MAX_RUN_EVENTS + ' AG-UI events in one run.');
          }
          let terminalIndex = -1;
          for (let index = 0; index < events.length; index++) {
            const event = events[index];
            if (event.type === 'RUN_STARTED' || event.type === 'RUN_FINISHED') {
              if (event.threadId !== input.threadId || event.runId !== input.runId) {
                throw new TypeError('The host returned an event for a different AG-UI run.');
              }
            }
            if (event.type === 'RUN_FINISHED' || event.type === 'RUN_ERROR') {
              if (index !== events.length - 1) {
                throw new TypeError('The host returned events after an AG-UI terminal event.');
              }
              terminalIndex = index;
            }
          }
          for (const event of events) subscriber.next(event);
          if (terminalIndex >= 0) {
            subscriber.complete();
            return;
          }
          afterSequence = page.sequence;
          // Status and events are separate requests. A terminal status may
          // describe events committed after the page just read: drain from its
          // cursor until the terminal event or an empty, stable tail page.
          if (terminalStatusObserved) {
            if (events.length === 0) {
              throw new TypeError('The host task ended without a terminal AG-UI event.');
            }
            continue;
          }
          if (typeof transport.status === 'function') {
            const task = await transport.status({runId: input.runId});
            if (!active || controller.signal.aborted) return;
            if (terminalTask(task)) {
              terminalStatusObserved = true;
              continue;
            }
          }
          await delay(pollIntervalMs, controller.signal, setTimeout);
        }
      } catch (error) {
        if (active && !controller.signal.aborted) subscriber.error(error);
      }
    };

    void poll();
    return () => {
      active = false;
      controller.abort();
    };
  });
}

export class LocalAppAgent extends AbstractAgent {
  constructor({transport, pollIntervalMs = 250, persistentChat = false, ...agentConfig}) {
    preflightJson(agentConfig.initialMessages || []);
    preflightJson(agentConfig.initialState ?? {});
    if (agentConfig.description !== undefined) preflightJson(agentConfig.description);
    super(agentConfig);
    if (!transport || typeof transport.start !== 'function' || typeof transport.events !== 'function') {
      throw new TypeError('LocalAppAgent needs start and events transport functions.');
    }
    if (!Number.isSafeInteger(pollIntervalMs) || pollIntervalMs < 10 || pollIntervalMs > 10000) {
      throw new RangeError('pollIntervalMs must be between 10 and 10000 milliseconds.');
    }
    this.transport = transport;
    this.pollIntervalMs = pollIntervalMs;
    this.persistentChat = persistentChat === true;
    this.sentMessageIds = new Set((agentConfig.initialMessages || []).map(message => message.id));
    this.lastRunId = undefined;
    this.pendingInput = undefined;
    this.readonlyConnecting = false;
  }

  prepareRunAgentInput(parameters) {
    preflightJson(parameters ?? {});
    preflightJson(this.messages);
    preflightJson(this.state);
    const prepared = super.prepareRunAgentInput(parameters);
    const input = this.pendingInput
      ? {...prepared, ...this.pendingInput, threadId: this.threadId, messages: this.messages}
      : prepared;
    this.pendingInput = undefined;
    return this.readonlyConnecting
      ? validateRunAgentInput(input)
      : localAppInput(input, {persistentChat: this.persistentChat, knownMessageIds: this.sentMessageIds});
  }

  addMessage(message) {
    preflightJson(message);
    super.addMessage(message);
  }

  addMessages(messages) {
    preflightJson(messages);
    super.addMessages(messages);
  }

  setMessages(messages) {
    preflightJson(messages);
    super.setMessages(messages);
  }

  setState(state) {
    preflightJson(state);
    super.setState(state);
  }

  run(input) {
    const knownMessageIds = new Set(this.sentMessageIds);
    const acceptedInput = localAppInput(input, {persistentChat: this.persistentChat, knownMessageIds});
    this.lastRunId = acceptedInput.runId;
    return eventStream({
      input: acceptedInput,
      transport: this.transport,
      start: true,
      knownMessageIds,
      pollIntervalMs: this.pollIntervalMs,
      persistentChat: this.persistentChat,
      onAccepted: () => {
        for (const message of acceptedInput.messages) this.sentMessageIds.add(message.id);
      },
    });
  }

  async runInput(input, subscriber) {
    const knownMessageIds = new Set(this.sentMessageIds);
    const acceptedInput = localAppInput(input, {persistentChat: this.persistentChat, knownMessageIds});
    this.threadId = acceptedInput.threadId;
    this.messages = structuredClone(acceptedInput.messages);
    this.state = structuredClone(acceptedInput.state ?? {});
    this.pendingInput = acceptedInput;
    let terminalOutcome;
    let runError;
    let runFailure;
    const wrappedSubscriber = {
      ...(subscriber || {}),
      async onRunFinishedEvent(params) {
        terminalOutcome = params.outcome;
        return await subscriber?.onRunFinishedEvent?.(params);
      },
      async onRunErrorEvent(params) {
        runError = params.event;
        return await subscriber?.onRunErrorEvent?.(params);
      },
    };
    const result = await this.runAgent({
      runId: acceptedInput.runId,
      tools: acceptedInput.tools,
      context: acceptedInput.context,
      forwardedProps: acceptedInput.forwardedProps,
      ...(acceptedInput.resume === undefined ? {} : {resume: acceptedInput.resume}),
    }, {
      ...wrappedSubscriber,
      async onRunFailed(params) {
        runFailure = params.error;
        const mutation = await subscriber?.onRunFailed?.(params);
        return {...(mutation || {}), stopPropagation: true};
      },
    }).catch(error => {
      if (runError) {
        const failure = new Error(runError.message || 'The AG-UI run failed.');
        failure.code = runError.code || 'agent_run_error';
        failure.event = runError;
        throw failure;
      }
      throw runFailure || error;
    });
    if (runError) {
      const error = new Error(runError.message || 'The AG-UI run failed.');
      error.code = runError.code || 'agent_run_error';
      error.event = runError;
      throw error;
    }
    if (runFailure) throw runFailure;
    if (terminalOutcome !== 'success') {
      const error = new Error(terminalOutcome === 'cancelled'
        ? 'The AG-UI run was cancelled.'
        : terminalOutcome === 'interrupt'
          ? 'The AG-UI run paused for an interrupt.'
          : 'The AG-UI run ended without a successful terminal event.');
      error.code = terminalOutcome === 'cancelled' ? 'run_cancelled'
        : terminalOutcome === 'interrupt' ? 'run_interrupted' : 'missing_terminal_event';
      throw error;
    }
    return result;
  }

  async connectAgent(parameters, subscriber) {
    const runId = parameters?.runId ?? this.lastRunId;
    if (!runId) return {result: undefined, newMessages: []};
    this.readonlyConnecting = true;
    try {
      const result = await super.connectAgent({...parameters, runId}, subscriber);
      for (const message of this.messages) this.sentMessageIds.add(message.id);
      return result;
    } finally {
      this.readonlyConnecting = false;
    }
  }

  connect(input) {
    if (!input?.runId) return EMPTY;
    return eventStream({
      input: validateRunAgentInput(input),
      transport: this.transport,
      start: false,
      knownMessageIds: this.sentMessageIds,
      persistentChat: this.persistentChat,
      pollIntervalMs: this.pollIntervalMs,
    });
  }

  abortRun() {
    const runId = this.lastRunId;
    if (runId && typeof this.transport.cancel === 'function') {
      void this.transport.cancel({runId});
    }
  }
}

export function createLocalAppAgent(config) {
  return new LocalAppAgent(config);
}
