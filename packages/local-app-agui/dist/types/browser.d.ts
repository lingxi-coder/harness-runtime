import type {BaseEvent, ContentPart, Message, RunAgentInput} from '@ag-ui/core';
import type {AgentConfig, RunAgentResult} from '@ag-ui/client';
import type {LocalAppAgent} from './client.js';

export interface ChatHistoryPage {
  messages: Message[];
  nextCursor?: string;
}

export interface LocalAppChatHandle {
  id: string;
  send(input: RunAgentInput): Promise<RunAgentResult>;
  history(options?: {cursor?: string; limit?: number}): Promise<ChatHistoryPage>;
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
  readonly chats: {
    create(options?: {title?: string}): Promise<LocalAppChatHandle>;
    open(chatId: string): Promise<LocalAppChatHandle>;
    list(options?: {cursor?: string; limit?: number; includeArchived?: boolean}): Promise<{chats: LocalAppChatSummary[]; nextCursor?: string}>;
    archive(chatId: string): Promise<void>;
    delete(chatId: string): Promise<void>;
  };
}

export interface BrowserAgentHostTransport {
  start(input: RunAgentInput, options?: {knownMessageIds?: ReadonlySet<string>; persistentChat?: boolean}): Promise<{runId: string}>;
  events(input: {runId: string; afterSequence?: number}): Promise<{events: BaseEvent[]; sequence: number}>;
  status?(input: {runId: string}): Promise<{state?: string}>;
  cancel?(input: {runId: string}): Promise<void>;
}

export interface BrowserAgentResources {
  tasks: LocalAppBrowserAgentResource['tasks'];
  chats: LocalAppBrowserAgentResource['chats'];
}

export declare function installBrowserAgent(options: {
  transport: BrowserAgentHostTransport;
  tasks: BrowserAgentResources['tasks'];
  chats: BrowserAgentResources['chats'];
  isPersistentThread?: (threadId: string) => boolean;
}): LocalAppBrowserAgentResource;
