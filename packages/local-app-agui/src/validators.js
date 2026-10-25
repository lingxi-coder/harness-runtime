import {
  ContentPartSchema,
  EventSchema,
  MessageSchema,
  RunAgentInputSchema,
} from '@ag-ui/core/schemas';
import {EVENT_PAGE_PREFLIGHT_LIMITS, preflightJson} from './preflight.js';

export function validateRunAgentInput(value) {
  preflightJson(value);
  return RunAgentInputSchema.parse(value);
}

export function validateEvent(value) {
  preflightJson(value, EVENT_PAGE_PREFLIGHT_LIMITS);
  return EventSchema.parse(value);
}

export function validateContentPart(value) {
  preflightJson(value);
  return ContentPartSchema.parse(value);
}

export function validateMessage(value) {
  preflightJson(value);
  return MessageSchema.parse(value);
}
