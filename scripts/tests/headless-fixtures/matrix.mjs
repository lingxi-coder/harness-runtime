// Declarative, local-provider-only scenarios. A result is observed as soon as it
// is emitted; EOF is sent after the last result, never before process cleanup.
export const schema = {type: 'object', properties: {answer: {type: 'string'}}, required: ['answer'], additionalProperties: false};
const simple = (id, format, extra = {}) => ({id, format, tools: '', turns: ['Return HEADLESS_LOCAL_RESPONSE.'], responses: [{text: 'HEADLESS_LOCAL_RESPONSE'}], ...extra});
export const fixtures = [
  simple('print-text', 'text'),
  simple('print-json', 'json'),
  simple('print-json-verbose', 'json', {verbose: true}),
  simple('print-json-verbose-tools', 'json', {verbose: true, tools: 'Bash', permission: 'manual', responses: [{tool: {name: 'Bash', input: {command: 'printf HEADLESS_TOOL > permission-marker.txt', description: 'Write the isolated fixture marker'}}}, {text: 'HEADLESS_DENIED'}]}),
  simple('print-json-verbose-hooks', 'json', {verbose: true, bare: false, startupHooks: true, flags: ['--include-hook-events']}),
  simple('print-json-verbose-shutdown', 'json', {verbose: true, bare: false, endHooks: true}),
  simple('print-stream-json', 'stream-json'),
  simple('partial-stream', 'stream-json', {flags: ['--include-partial-messages'], responses: [{text: 'HEADLESS_LOCAL_RESPONSE', chunks: ['HEADLESS_', 'LOCAL_', 'RESPONSE']}]}),
  simple('unicode-json', 'json', {responses: [{text: 'UTF16 Ω 😀 e\u0301\n"quoted" \\ slash'}]}),
  simple('stdin-text', 'text', {textStdin: true}),
  simple('no-persistence', 'stream-json', {flags: ['--no-session-persistence'], persistence: false}),
  simple('sdk-initialize', 'stream-json', {input: 'stream-json', initialize: true}),
  simple('sdk-replay', 'stream-json', {input: 'stream-json', flags: ['--replay-user-messages']}),
  simple('sdk-multi-turn', 'stream-json', {input: 'stream-json', turns: ['First local turn.', 'Second local turn.'], responses: [{text: 'HEADLESS_FIRST'}, {text: 'HEADLESS_SECOND'}]}),
  simple('sdk-resume', 'stream-json', {input: 'stream-json', resume: true, responses: [{text: 'HEADLESS_LOCAL_RESPONSE'}, {text: 'HEADLESS_RESUMED_RESPONSE'}]}),
  simple('sdk-fork', 'stream-json', {input: 'stream-json', resume: true, resumeFork: true, responses: [{text: 'HEADLESS_LOCAL_RESPONSE'}, {text: 'HEADLESS_FORKED_RESPONSE'}]}),
  simple('schema-success', 'json', {flags: ['--json-schema', JSON.stringify(schema)], responses: [{tool: {name: 'StructuredOutput', input: {answer: 'HEADLESS_SCHEMA_RESPONSE'}}}, {text: ''}]}),
  simple('schema-retry-exhausted', 'json', {expectedExit: 1, expectedSubtype: 'error_max_structured_output_retries', env: {MAX_STRUCTURED_OUTPUT_RETRIES: '2'}, flags: ['--json-schema', JSON.stringify(schema)], responses: Array.from({length: 8}, () => ({tool: {name: 'StructuredOutput', input: {answer: 42}}}))}),
  simple('max-turns', 'stream-json', {tools: 'Bash', expectedExit: 1, expectedSubtype: 'error_max_turns', flags: ['--max-turns', '1', '--allowedTools=Bash'], responses: [{tool: {name: 'Bash', input: {command: 'printf HEADLESS_TOOL', description: 'Print the local fixture marker'}}}, {text: 'HEADLESS_AFTER_TOOL'}]}),
  simple('budget-limit', 'json', {expectedExit: 1, expectedSubtype: 'error_max_budget_usd', flags: ['--max-budget-usd', '0.000001'], responses: [{text: 'HEADLESS_BUDGET_RESPONSE', usage: {input_tokens: 1000, output_tokens: 1000}}]}),
  simple('permission-deny-none', 'stream-json', {tools: 'Bash', permission: 'manual', prompts: 'none', responses: [{tool: {name: 'Bash', input: {command: 'printf HEADLESS_TOOL > permission-marker.txt', description: 'Write the isolated fixture marker'}}}, {text: 'HEADLESS_DENIED'}]}),
  simple('permission-host-allow', 'stream-json', {tools: 'Bash', input: 'stream-json', initialize: true, permission: 'manual', prompts: 'host', approval: 'allow', flags: ['--permission-prompt-tool', 'stdio'], responses: [{tool: {name: 'Bash', input: {command: 'printf HEADLESS_TOOL > permission-marker.txt', description: 'Write the isolated fixture marker'}}}, {text: 'HEADLESS_ALLOWED'}]}),
  simple('permission-host-deny', 'stream-json', {tools: 'Bash', input: 'stream-json', initialize: true, permission: 'manual', prompts: 'host', approval: 'deny', flags: ['--permission-prompt-tool', 'stdio'], responses: [{tool: {name: 'Bash', input: {command: 'printf HEADLESS_TOOL > permission-marker.txt', description: 'Write the isolated fixture marker'}}}, {text: 'HEADLESS_DENIED'}]}),
  simple('sdk-interrupt', 'stream-json', {input: 'stream-json', initialize: true, interrupt: true, expectedExit: 1, expectedSubtype: 'error_during_execution', responses: [{text: 'HEADLESS_CANCELLED_RESPONSE', delayMs: 2000}]}),
  simple('sdk-malformed-input', 'stream-json', {input: 'stream-json', malformed: true, responses: []}),
  simple('sdk-control-validation', 'stream-json', {input: 'stream-json', initialize: true, controls: [
    {id: 'read-missing', request: {subtype: 'mcp_read_resource'}},
    {id: 'read-bad-uri', request: {subtype: 'mcp_read_resource', serverName: 'missing', uri: 'https://example.invalid/fixture'}},
    {id: 'read-unknown-server', request: {subtype: 'mcp_read_resource', serverName: 'missing', uri: 'ui://headless-fixture/readme'}},
    {id: 'send-now-idle', request: {subtype: 'interrupt', send_now: true}},
    {id: 'send-now-unknown', request: {subtype: 'interrupt', send_now: true, message_uuid: '00000000-0000-4000-8000-000000000001'}},
    {id: 'cancel-async-unknown', request: {subtype: 'cancel_async_message', message_uuid: '00000000-0000-4000-8000-000000000001'}},
    {id: 'ui-attach', request: {subtype: 'ui_attach', surface: 'mobile', client_id: 'headless-fixture-client'}},
    {id: 'ui-detach', request: {subtype: 'ui_detach', client_id: 'headless-fixture-client'}},
  ]}),
  simple('startup-hooks-mcp', 'stream-json', {bare: false, input: 'stream-json', initialize: true, startupHooks: true, mcp: true, providerDelayMs: 200, flags: ['--include-hook-events'], controls: [{id: 'read-local-resource-pending', request: {subtype: 'mcp_read_resource', serverName: 'fixture', uri: 'ui://headless-fixture/readme'}}], afterResultControls: [{id: 'read-local-resource-connected', request: {subtype: 'mcp_read_resource', serverName: 'fixture', uri: 'ui://headless-fixture/readme'}}]}),
  simple('startup-local-plugin', 'stream-json', {bare: false, localPlugin: true}),
];

// Explicitly selected research probes. Unknown/error outcomes are collected;
// these are excluded from the default acceptance matrix until reviewed.
const invalidStructured = Array.from({length: 8}, () => ({tool: {name: 'StructuredOutput', input: {answer: 42}}}));
export const schemaProbes = [
  simple('probe-schema-no-tool', 'json', {probe: true, env: {MAX_STRUCTURED_OUTPUT_RETRIES: '2'}, flags: ['--json-schema', JSON.stringify(schema)], responses: Array.from({length: 8}, () => ({text: 'HEADLESS_SCHEMA_WITHOUT_TOOL'}))}),
  ...[['zero', '0'], ['negative', '-1'], ['prefix', '2tail'], ['nan', 'nope']].map(([label, value]) => simple(`probe-schema-max-${label}`, 'json', {probe: true, env: {MAX_STRUCTURED_OUTPUT_RETRIES: value}, flags: ['--json-schema', JSON.stringify(schema)], responses: invalidStructured})),
  ...['1', '2'].map(value => simple(`probe-schema-success-turns-${value}`, 'json', {probe: true, flags: ['--json-schema', JSON.stringify(schema), '--max-turns', value], responses: [{tool: {name: 'StructuredOutput', input: {answer: 'HEADLESS_SCHEMA_RESPONSE'}}}, {text: ''}]})),
  simple('probe-schema-success-text', 'text', {probe: true, flags: ['--json-schema', JSON.stringify(schema)], responses: [{tool: {name: 'StructuredOutput', input: {answer: 'HEADLESS_SCHEMA_RESPONSE'}}}, {text: ''}]}),
];

// Native lifecycle research remains separate from the accepted parity matrix.
export const lifecycleProbes = [
  simple('probe-input-midtool-burst', 'stream-json', {probe: true, input: 'stream-json', initialize: true, inputSchedule: 'after-tool-burst', acceptedResultCounts: [1, 2, 3], userUuids: ['00000000-0000-4000-8000-000000000051', '00000000-0000-4000-8000-000000000052', '00000000-0000-4000-8000-000000000053'], turns: ['HEADLESS_MULTIFOLD_A', 'HEADLESS_MULTIFOLD_B', 'HEADLESS_MULTIFOLD_C'], tools: 'Bash', flags: ['--include-partial-messages', '--replay-user-messages', '--allowedTools=Bash'], responses: [{tool: {name: 'Bash', input: {command: 'sleep 0.2; printf HEADLESS_TOOL', description: 'Controlled local multi-input queue window'}}}, {text: 'HEADLESS_MULTIFOLD_RESPONSE'}, {text: 'HEADLESS_MULTIFOLD_SECOND_RESPONSE'}, {text: 'HEADLESS_MULTIFOLD_THIRD_RESPONSE'}]}),
  simple('probe-parser-tools-default', 'json', {probe: true, tools: 'default'}),
  simple('probe-parser-mixed-stdin', 'json', {probe: true, stdinText: 'HEADLESS_STDIN_WITH_LF\n', turns: ['HEADLESS_ARGV_WITHOUT_LF']}),
  simple('probe-parser-partial-json', 'json', {probe: true, expectedResultCount: 0, flags: ['--include-partial-messages']}),
  simple('probe-parser-replay-text', 'text', {probe: true, flags: ['--replay-user-messages']}),
  simple('probe-parser-sdk-json', 'json', {probe: true, input: 'stream-json', expectedResultCount: 0, parserFailureProbe: true}),
  simple('probe-input-burst', 'stream-json', {probe: true, input: 'stream-json', initialize: true, inputSchedule: 'burst', acceptedResultCounts: [1, 2], userUuids: ['00000000-0000-4000-8000-000000000031', '00000000-0000-4000-8000-000000000032'], turns: ['HEADLESS_BATCH_A', 'HEADLESS_BATCH_B'], flags: ['--include-partial-messages', '--replay-user-messages'], responses: [{text: 'HEADLESS_BATCH_RESPONSE'}, {text: 'HEADLESS_BATCH_SECOND_RESPONSE'}]}),
  simple('probe-input-midtool', 'stream-json', {probe: true, input: 'stream-json', initialize: true, inputSchedule: 'after-tool-frame', acceptedResultCounts: [1, 2], userUuids: ['00000000-0000-4000-8000-000000000041', '00000000-0000-4000-8000-000000000042'], turns: ['HEADLESS_MIDTOOL_A', 'HEADLESS_MIDTOOL_B'], tools: 'Bash', flags: ['--include-partial-messages', '--replay-user-messages', '--allowedTools=Bash'], responses: [{tool: {name: 'Bash', input: {command: 'sleep 0.2; printf HEADLESS_TOOL', description: 'Controlled local queue window'}}}, {text: 'HEADLESS_MIDTOOL_RESPONSE'}, {text: 'HEADLESS_MIDTOOL_SECOND_RESPONSE'}]}),
  simple('probe-client-markers-max-turns', 'stream-json', {probe: true, input: 'stream-json', userUuids: ['00000000-0000-4000-8000-000000000023'], tools: 'Bash', flags: ['--include-partial-messages', '--max-turns', '1', '--allowedTools=Bash'], responses: [{tool: {name: 'Bash', input: {command: 'printf HEADLESS_TOOL', description: 'Controlled marker limit'}}}]}),
  simple('probe-client-markers-success', 'stream-json', {probe: true, input: 'stream-json', userUuids: ['00000000-0000-4000-8000-000000000021'], tools: 'Bash', flags: ['--include-partial-messages', '--allowedTools=Bash'], responses: [{tool: {name: 'Bash', input: {command: 'printf HEADLESS_TOOL', description: 'Controlled marker continuation'}}}, {text: 'HEADLESS_MARKER_COMPLETE'}]}),
  simple('probe-client-markers-http-error', 'stream-json', {probe: true, input: 'stream-json', userUuids: ['00000000-0000-4000-8000-000000000022'], flags: ['--include-partial-messages'], responses: [{httpError: {status: 400, body: {type: 'error', error: {type: 'invalid_request_error', message: 'HEADLESS_LOCAL_PROVIDER_ERROR'}}}}]}),
  simple('probe-mcp-ui-metadata', 'stream-json', {probe: true, input: 'stream-json', initialize: true, initializeRequest: {sdkMcpServers: ['fixture'], sdkMcpServerManifests: {fixture: {initializeResult: {protocolVersion: '2025-11-25', serverInfo: {name: 'headless-sdk-fixture', version: '1.0.0'}, capabilities: {tools: {}}}, toolsListResult: {tools: [{name: 'uiTool', description: 'Controlled metadata-only SDK tool', inputSchema: {type: 'object', properties: {}}, _meta: {ui: {resourceUri: 'ui://fixture/view', visibility: ['model', 'app'], custom: 'x'}, 'ui/resourceUri': 'ui://fixture/legacy', private: true}}]}}}}, controls: [{id: 'mcp-status-ui-meta', request: {subtype: 'mcp_status'}}]}),
  simple('probe-safety-refusal', 'stream-json', {probe: true, responses: [{text: 'HEADLESS_CONTROLLED_REFUSAL', stopReason: 'refusal'}, {text: 'HEADLESS_AFTER_REFUSAL'}]}),
  simple('probe-background-agent', 'stream-json', {probe: true, expectedResultCount: 2, bare: false, input: 'stream-json', initialize: true, tools: 'Agent', requirePermissionRequest: false, approval: 'allow', permission: 'manual', prompts: 'host', flags: ['--permission-prompt-tool', 'stdio', '--allowedTools=Agent'], turns: ['HEADLESS_BACKGROUND_PARENT: start the requested background Agent and then finish.'], responseRoutes: true, maxProviderRequests: 8, responses: [
    {match: {lastUserTextIncludes: 'HEADLESS_BACKGROUND_PARENT'}, tool: {name: 'Agent', input: {description: 'Controlled local background child', subagent_type: 'general-purpose', prompt: 'HEADLESS_BACKGROUND_CHILD: return the controlled child marker.', run_in_background: true}}},
    {match: {lastUserTextIncludes: 'HEADLESS_BACKGROUND_CHILD'}, text: 'HEADLESS_CHILD_COMPLETE', delayMs: 200},
    {match: {lastUserHasToolResult: true}, text: 'HEADLESS_PARENT_COMPLETE'},
    {match: {lastUserTextIncludes: '<task-notification>'}, text: 'HEADLESS_AFTER_CHILD_NOTIFICATION'},
  ]}),
];

lifecycleProbes.push(
  simple('probe-text-provider-error', 'text', {probe: true, responses: [{httpError: {status: 400, body: {type: 'error', error: {type: 'invalid_request_error', message: 'HEADLESS_LOCAL_PROVIDER_ERROR'}}}}]}),
  simple('probe-json-provider-error', 'json', {probe: true, responses: [{httpError: {status: 400, body: {type: 'error', error: {type: 'invalid_request_error', message: 'HEADLESS_LOCAL_PROVIDER_ERROR'}}}}]}),
  {...fixtures.find(fixture => fixture.id === 'max-turns'), id: 'probe-text-max-turns', format: 'text', probe: true, expectedSubtype: undefined},
  simple('probe-text-schema-error', 'text', {probe: true, env: {MAX_STRUCTURED_OUTPUT_RETRIES: '0'}, flags: ['--json-schema', JSON.stringify(schema)], responses: [{tool: {name: 'StructuredOutput', input: {answer: 42}}}]}),
);
const backgroundAgentProbe = lifecycleProbes.find(fixture => fixture.id === 'probe-background-agent');
lifecycleProbes.push(...[
  ['probe-background-agent-json', 'json', false],
  ['probe-background-agent-json-verbose', 'json', true],
  ['probe-background-agent-text', 'text', false],
].map(([id, format, verbose]) => ({...backgroundAgentProbe, id, format, verbose, input: undefined, initialize: false, approval: undefined, permission: 'dontAsk', prompts: 'none', flags: ['--allowedTools=Agent'], expectedResultCount: verbose ? 2 : 1})));

export function validateFixtures(selected = fixtures) {
  const ids = new Set();
  for (const fixture of selected) {
    if (!/^[a-z0-9-]+$/.test(fixture.id) || ids.has(fixture.id)) throw new Error(`Invalid/duplicate fixture id ${fixture.id}`);
    ids.add(fixture.id);
    if (!['text', 'json', 'stream-json'].includes(fixture.format)) throw new Error(`Invalid output format ${fixture.id}`);
    if (fixture.input === 'stream-json' && fixture.format !== 'stream-json' && !(fixture.probe && fixture.parserFailureProbe)) throw new Error(`SDK requires stream-json ${fixture.id}`);
    if (fixture.stdinText !== undefined && (typeof fixture.stdinText !== 'string' || fixture.input === 'stream-json')) throw new Error(`Invalid plain stdin ${fixture.id}`);
    if (!Array.isArray(fixture.responses) || !Array.isArray(fixture.turns) || fixture.turns.length === 0) throw new Error(`Missing responses/turns ${fixture.id}`);
    if (fixture.inputSchedule && (!fixture.probe || fixture.input !== 'stream-json' || !['burst', 'after-tool-frame', 'after-tool-burst'].includes(fixture.inputSchedule))) throw new Error(`Invalid research input schedule ${fixture.id}`);
    if (fixture.acceptedResultCounts && (!fixture.probe || !Array.isArray(fixture.acceptedResultCounts) || fixture.acceptedResultCounts.some(value => !Number.isInteger(value) || value < 1 || value > fixture.turns.length))) throw new Error(`Invalid research result counts ${fixture.id}`);
    if (fixture.responseRoutes && (!Number.isInteger(fixture.maxProviderRequests) || fixture.maxProviderRequests < 1 || fixture.maxProviderRequests > 16)) throw new Error(`Response routes require a bounded 1..16 request limit ${fixture.id}`);
    // Controlled command is constant and has no path, network, shell expansion,
    // or access to the user's files. A fixture cannot inject arbitrary shell.
    for (const response of fixture.responses) {
      if (response.tool?.name === 'Bash' && !['printf HEADLESS_TOOL', 'printf HEADLESS_TOOL > permission-marker.txt', 'sleep 0.2; printf HEADLESS_TOOL'].includes(response.tool.input.command)) throw new Error(`Uncontrolled command ${fixture.id}`);
    }
  }
  return selected;
}
