import type {Observable} from 'rxjs';
import type {AbstractAgent, AgentConfig, AgentSubscriber, RunAgentParameters, RunAgentResult} from '@ag-ui/client';
import type {BaseEvent, Message, RunAgentInput} from '@ag-ui/core';

export declare const LOCAL_APP_EXTENSION_KEY: 'com.lingxi.localApp';
export declare const AG_UI_PROTOCOL_VERSION: '1.0';

export interface AgentEventPage {
  events: BaseEvent[];
  sequence: number;
}

export interface LocalAppAgentTransport {
  start(input: RunAgentInput, options?: {knownMessageIds?: ReadonlySet<string>; persistentChat?: boolean}): Promise<{runId: string}>;
  events(input: {runId: string; afterSequence?: number}, options?: {signal?: AbortSignal}): Promise<AgentEventPage>;
  status?(input: {runId: string}): Promise<{state?: string}>;
  cancel?(input: {runId: string}): Promise<void>;
}

export interface LocalAppAgentConfig extends AgentConfig {
  transport: LocalAppAgentTransport;
  pollIntervalMs?: number;
  persistentChat?: boolean;
}

export declare class LocalAppAgent extends AbstractAgent {
  constructor(config: LocalAppAgentConfig);
  run(input: RunAgentInput): Observable<BaseEvent>;
  runInput(input: RunAgentInput, subscriber?: AgentSubscriber): Promise<RunAgentResult>;
  connectAgent(parameters?: RunAgentParameters, subscriber?: AgentSubscriber): Promise<RunAgentResult>;
  abortRun(): void;
}

export declare function createLocalAppAgent(config: LocalAppAgentConfig): LocalAppAgent;
export declare function validateLocalAppRunInput(value: unknown, options?: {persistentChat?: boolean; knownMessageIds?: ReadonlySet<string>}): RunAgentInput;
export type {AbstractAgent, AgentConfig, AgentSubscriber, BaseEvent, Message, RunAgentInput, RunAgentParameters, RunAgentResult};
export {ContentPartSchema, EventSchema, MessageSchema, RunAgentInputSchema} from '@ag-ui/core/schemas';
export {validateContentPart, validateEvent, validateMessage, validateRunAgentInput} from './validators.js';
