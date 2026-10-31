import type {BaseEvent, ContentPart, Message, RunAgentInput} from '@ag-ui/core';
import type {AgentConfig, RunAgentResult} from '@ag-ui/client';
import type {LocalAppAgent} from './client.js';

export interface ChatHistoryPage {
  messages: Message[];
  nextCursor?: string;
  /** Mobile host recovery frontier; changed values require refreshing canonical history. */
  recoveryGeneration?: string;
  /** The host proved that the supplied cursor belonged to a sealed attempt. */
  cursorReset?: true;
}

export interface LocalAppChatHandle {
  id: string;
  send(input: RunAgentInput): Promise<RunAgentResult>;
  history(options?: {cursor?: string; limit?: number}): Promise<ChatHistoryPage>;
  /** Mobile hosts expose retained failed-attempt inputs without resubmitting the run. */
  attempts?: {
    (options: {runId: string; cursor?: string; limit?: number}): Promise<ChatHistoryPage>;
    (options?: {cursor?: string; limit?: number}): Promise<{attempts: LocalAppTask[]}>;
  };
  subscribe(listener: (message: Message) => void): () => void;
  cancel(): Promise<void>;
}

export interface LocalAppChatSummary {
  chatId: string;
  title: string;
  createdAt: number;
  updatedAt: number;
  archived: boolean;
  state: 'idle' | 'running' | 'interrupted';
  [key: string]: unknown;
}

export type LocalAppTaskState = 'queued' | 'running' | 'succeeded' | 'failed' | 'cancelled' | 'interrupted';

/** Public task metadata. Inputs, model results, and private task ledger fields are excluded. */
export interface LocalAppTask {
  runId: string;
  threadId: string;
  state: LocalAppTaskState;
  sequence: number;
  phase?: string;
  progress?: {completedModelTurns: number; turnLimit: number};
  createdAt: number;
  updatedAt: number;
  error?: {code: string; message: string};
}

export interface LocalAppTaskPage {
  tasks: LocalAppTask[];
  nextCursor?: string;
}

export interface LocalAppChatCreationOptions {
  /** A caller-generated, lowercase canonical UUIDv4, persisted before submission. */
  requestId: string;
  title?: string;
}

export interface LocalAppPendingChatCreation extends LocalAppChatCreationOptions {
  state: 'pending' | 'accepted' | 'deleted' | 'rejected';
}

interface LocalAppCommonChats {
  open(chatId: string): Promise<LocalAppChatHandle>;
  list(options?: {cursor?: string; limit?: number; includeArchived?: boolean}): Promise<{
    chats: LocalAppChatSummary[];
    /** Continues the combined ordering of available and unavailable Chats. */
    nextCursor?: string;
    /** Unavailable Chat IDs in this page, when the host reports them. */
    unavailableChats?: Array<{chatId: string; code: 'chat_storage_uncertain'}>;
    /** True if any Chat is unavailable across the App; absent means not reported. */
    hasUncertainChats?: boolean;
  }>;
  archive(chatId: string): Promise<void>;
  delete(chatId: string): Promise<void>;
}

/** LingXi host extension, separate from the AG-UI run protocol. */
export interface LocalAppChats extends LocalAppCommonChats {
  /**
   * Invoke only for a user-requested creation or explicit same-input retry.
   * Mobile saves the request before submission and coalesces same-page calls.
   * For an unknown prior outcome, a trusted SDK confirmation is required to
   * send a retry. Stopping a browser wait does not determine the host outcome.
   */
  create(options: LocalAppChatCreationOptions): Promise<LocalAppChatHandle>;
  /** Null means not currently observed; an earlier request may still arrive. */
  creationStatus(options: LocalAppChatCreationOptions): Promise<LocalAppChatHandle | null>;
  /**
   * Reads receipts for this App and revision. On Mobile, a fresh page may
   * reconcile pending receipts through bounded, read-only status queries;
   * this list can be read again after those queries complete.
   */
  pendingCreations(): Promise<LocalAppPendingChatCreation[]>;
  /** Removes a resolved browser receipt, not the host Chat. */
  forgetCreation(requestId: string): Promise<void>;
}

export interface LocalAppBrowserAgentResource {
  run(input: RunAgentInput): Promise<RunAgentResult>;
  createClient(options?: AgentConfig): LocalAppAgent;
  file(file: File): Promise<ContentPart>;
  readonly tasks: {
    start(input: RunAgentInput): Promise<{runId: string}>;
    events(input: {runId: string; afterSequence?: number}): Promise<{events: BaseEvent[]; sequence: number}>;
    subscribe(runId: string, listener: (event: BaseEvent) => void): () => void;
    get(runId: string): Promise<LocalAppTask>;
    list(options?: {cursor?: string; limit?: number}): Promise<LocalAppTaskPage>;
    cancel(runId: string): Promise<void>;
  };
  readonly chats: LocalAppChats;
}

export interface BrowserAgentHostTransport {
  start(input: RunAgentInput, options?: {knownMessageIds?: ReadonlySet<string>; persistentChat?: boolean}): Promise<{runId: string}>;
  events(input: {runId: string; afterSequence?: number}): Promise<{events: BaseEvent[]; sequence: number}>;
  status?(input: {runId: string}): Promise<{state?: string}>;
  cancel?(input: {runId: string}): Promise<void>;
}

export interface BrowserAgentResources {
  tasks: LocalAppBrowserAgentResource['tasks'];
  chats: LocalAppChats;
}

export declare function installBrowserAgent(options: {
  transport: BrowserAgentHostTransport;
  tasks: BrowserAgentResources['tasks'];
  chats: LocalAppChats;
  isPersistentThread?: (threadId: string) => boolean;
}): LocalAppBrowserAgentResource;
