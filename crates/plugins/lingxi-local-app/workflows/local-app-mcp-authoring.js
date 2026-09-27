// Per-App MCP authoring (lingxi-local-app:local-app-mcp-authoring).
// The workflow owns orchestration; Host-owned tools remain the authority for
// validation, receipts, candidate persistence and promotion.
export const meta = {
  name: 'local-app-mcp-authoring',
  description: 'Author, validate, QA and promote one Local App MCP catalog with Host-bound evidence.',
  phases: [{ title: 'Evidence and proposal' }, { title: 'Validate and approve' }, { title: 'QA and promote' }],
};

const WORKFLOW_ID = 'lingxi-local-app:local-app-mcp-authoring';
const EXTERNAL_KEYS = ['app_id', 'user_goal'];
// `host_context` is injected by the trusted Host; every other top-level key
// is user/model input and must be rejected, including operation/quality/run
// identity claims.
const INTERNAL_KEYS = ['host_context', 'workflow_run_id', 'runtime_profile', 'expected_writable_collections'];
const FORBIDDEN = ['tools', 'tool_definitions', 'server_name', 'annotations', 'permission_rules', 'flow_id', 'workspace_path', 'proposal_digest', 'catalog_digest', 'digests'];
const QUALITY = ['fast', 'balanced', 'thorough'];
const input = args && typeof args === 'object' && !Array.isArray(args) ? args : {};
const unknown = Object.keys(input).filter((key) => !EXTERNAL_KEYS.includes(key) && !INTERNAL_KEYS.includes(key));
if (unknown.length) throw new Error(`${WORKFLOW_ID}: external input is limited to app_id,user_goal; unknown field(s): ${unknown.join(', ')}`);
const forged = FORBIDDEN.filter((key) => Object.prototype.hasOwnProperty.call(input, key));
if (forged.length) throw new Error(`${WORKFLOW_ID}: rejects caller-supplied Host-owned field(s): ${forged.join(', ')}`);
if (typeof input.app_id !== 'string' || !input.app_id.trim()) throw new Error(`${WORKFLOW_ID}: app_id is required`);
if (typeof input.user_goal !== 'string' || !input.user_goal.trim()) throw new Error(`${WORKFLOW_ID}: user_goal is required`);
const context = input.host_context;
if (!context || context.source !== 'verified_host' || context.app_id !== input.app_id) throw new Error(`${WORKFLOW_ID}: HOST_CONTEXT_REQUIRED: Host must inject AppEvidence and run identity`);
if (typeof context.workflow_run_id !== 'string' || !context.workflow_run_id.trim()) throw new Error(`${WORKFLOW_ID}: HOST_CONTEXT_MISSING_RUN`);
if (typeof context.invocation_capability !== 'string' || !/^mcpv_[A-Za-z0-9]{32}$/.test(context.invocation_capability)) throw new Error(`${WORKFLOW_ID}: HOST_INVOCATION_CAPABILITY_REQUIRED: only a Host-minted workflow capability may start authoring`);
const operation = context.operation || 'initial';
const quality = context.quality_level || 'balanced';
if (!QUALITY.includes(quality)) throw new Error(`${WORKFLOW_ID}: quality_level must be fast, balanced, or thorough`);
const shared = context.schemas;
if (!shared || typeof shared !== 'object') throw new Error(`${WORKFLOW_ID}: HOST_SCHEMAS_REQUIRED`);
const schemaFor = (name) => {
  const schema = shared[name];
  if (!schema || typeof schema !== 'object') throw new Error(`${WORKFLOW_ID}: HOST_SCHEMA_MISSING: ${name}`);
  return schema;
};
const proposalSchema = schemaFor('mcp_proposal');
const promoterSchema = schemaFor('mcp_promoter');
const qaSchema = {
  type: 'object', properties: { ok: { type: 'boolean' }, findings: { type: 'array' }, mcp_schema: { type: 'string' }, flow_binding: { type: 'string' }, calls: { type: 'string' }, isolation: { type: 'string' }, verification_sha256: { type: 'string' }, summary: { type: 'string' } },
  required: ['ok', 'findings', 'mcp_schema', 'flow_binding', 'calls', 'isolation', 'verification_sha256', 'summary'], additionalProperties: false,
};
const objectResult = (value, stage) => {
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error(`${WORKFLOW_ID}: ${stage} returned malformed structured output`);
  return value;
};
const run = async (prompt, options) => objectResult(await agent(prompt, { ...options, workflowId: WORKFLOW_ID, throwOnError: true }), options.agentType);

// Evidence is assembled by the Host, not from caller paths or model claims.
phase('Evidence and proposal');
const evidence = await run(`Read Host-owned AppEvidence for app ${input.app_id}. Use LocalAppGet and the app's own bounded data only. Return persisted profile/build/catalog identities without inventing paths, digests or server metadata. Host context: ${JSON.stringify(context)}`, { agentType: 'mcp-designer', label: 'app-evidence', phase: 'Evidence and proposal', schema: { type: 'object', properties: { summary: { type: 'string' }, runtime_profile: { type: 'object' }, collections: { type: 'array' }, active_catalog: { type: ['object', 'null'] } }, required: ['summary', 'runtime_profile', 'collections', 'active_catalog'], additionalProperties: false } });
const proposal = await run(`Design an AppMcpProposal for app ${input.app_id} and user goal ${input.user_goal}. Match the Host DTO exactly: return only appId, manifestRevision, userGoalSha256, summary, tools, requiredFlowChanges, and excludedCapabilities. Each tool may contain name, title, description, inputSchema, outputSchema, semanticFlowId, inputs, and result. Flow bindings are externally tagged snake_case values such as {"tool_input":{"json_pointer":"/query"}} and {"step_output":{"step_id":"search","json_pointer":"/items"}}. Do not return server, annotations, icons, execution, _meta, permissions, ceiling, paths, receipts or catalog digests. If a meaningful bounded Flow is unavailable, return requiredFlowChanges or excludedCapabilities instead of inventing one. Evidence: ${JSON.stringify(evidence)}`, { agentType: 'mcp-designer', label: 'mcp-designer', phase: 'Evidence and proposal', schema: proposalSchema });
if (proposal.appId !== input.app_id) throw new Error(`${WORKFLOW_ID}: proposal appId does not match Host-bound app`);
if (Array.isArray(proposal.requiredFlowChanges) && proposal.requiredFlowChanges.length) return { status: 'needs_input', workflow_id: WORKFLOW_ID, app_id: input.app_id, required_flow_changes: proposal.requiredFlowChanges, proposal };

// Host re-reads the flow and derives all non-agent fields before approval.
phase('Validate and approve');
const validated = await run(`Use LocalAppValidateMcpProposal for app ${input.app_id}, workflow run ${context.workflow_run_id}. Host must re-read trusted Flow contexts, reject cross-app/forward/dynamic/forbidden bindings, derive permission ceiling and execution metadata, calculate canonical proposal_sha256, approval_contract_sha256 and tool_surface_sha256, and persist stage prepared. Never accept model-supplied server, annotations, permissions, paths or catalog identity. Return only ok, status, proposal_sha256, approval_contract_sha256, tool_surface_sha256 and findings; the Host review_surface is deliberately not part of this closed agent result. Proposal: ${JSON.stringify(proposal)}`, { agentType: 'mcp-designer', label: 'host-proposal-validation', phase: 'Validate and approve', schema: { type: 'object', properties: { ok: { type: 'boolean' }, status: { type: 'string' }, proposal_sha256: { type: 'string' }, approval_contract_sha256: { type: 'string' }, tool_surface_sha256: { type: 'string' }, findings: { type: 'array' } }, required: ['ok', 'status', 'findings'], additionalProperties: false } });
if (validated.ok !== true) throw new Error(`${WORKFLOW_ID}: Host rejected proposal: ${JSON.stringify(validated.findings || validated)}`);
if (!Array.isArray(proposal.tools) || proposal.tools.length === 0) return { status: 'mcp_authoring_required', workflow_id: WORKFLOW_ID, app_id: input.app_id, candidate_preserved: true, proposal, validation: validated, reason: 'No meaningful Flow-bound tool cleared authoring; Host retains the prepared candidate without receipt or promotion.' };
const approved = validated.status === 'approved_reusable'
  ? { approved: true, status: 'approved_reusable', receipt_id: null }
  : await run(`Use LocalAppApproveMcpProposal for app ${input.app_id}, workflow run ${context.workflow_run_id}. Approve only the Host-generated review surface and mint one promote receipt for this prepared candidate. Source bytes/build ids are not review-surface changes; any tool/schema/Flow/ceiling change requires a new diff and receipt. Validation: ${JSON.stringify(validated)}`, { agentType: 'mcp-designer', label: 'native-approval', phase: 'Validate and approve', schema: { type: 'object', properties: { approved: { type: 'boolean' }, receipt_id: { type: ['string', 'null'] }, status: { type: 'string' } }, required: ['approved', 'status', 'receipt_id'], additionalProperties: false } });
if (approved.approved !== true) return { status: 'approval_required', workflow_id: WORKFLOW_ID, app_id: input.app_id, proposal, validation: validated };
phase('QA and promote');
const qa = await run(`Use LocalAppQaMcpCandidate for app ${input.app_id}, workflow run ${context.workflow_run_id}. Re-read the approved candidate, validate schema limits, typed Flow binding, build identity, bounded call contract, and app isolation. UI evidence being unavailable must be reported as unverified, not used to bypass MCP QA. Mark the durable journal smoke_passed then mcp_verified only after the Host gates pass. Return only the closed QA summary fields in the supplied schema; do not copy the Host's detailed tool_evidence or isolation_evidence into this normalized agent result. Candidate: ${JSON.stringify(validated)}`, { agentType: 'tester', label: 'mcp-qa', phase: 'QA and promote', schema: qaSchema });
if (qa.ok !== true || qa.mcp_schema !== 'passed' || qa.flow_binding !== 'passed' || qa.calls !== 'passed' || qa.isolation !== 'passed') throw new Error(`${WORKFLOW_ID}: MCP QA failed closed: ${JSON.stringify(qa.findings)}`);
const promotionArguments = { app_id: input.app_id, workflow_run_id: context.workflow_run_id };
if (approved.receipt_id !== null) promotionArguments.receipt_id = approved.receipt_id;
const promoted = await run(`Use LocalAppPromoteMcpCandidate exactly once with this exact input object and no other arguments: ${JSON.stringify(promotionArguments)}. Do not pass the candidate, validation, QA result, catalog digest, or any Host response field; the Host reloads the sealed candidate and QA journal. Atomically promote the non-empty catalog, preserving the previous build/catalog pair on failure. Return only promoted, status, publication_state, and catalog_sha256 when the Host supplies it; do not blindly forward extra response fields into the closed result.`, { agentType: 'mcp-promoter', label: 'mcp-promote', phase: 'QA and promote', schema: promoterSchema });
if (promoted.promoted !== true) throw new Error(`${WORKFLOW_ID}: Host did not promote MCP candidate: ${promoted.status}`);
return { status: 'promoted', workflow_id: WORKFLOW_ID, app_id: input.app_id, operation, quality_level: quality, proposal, validation: validated, qa, promotion: promoted };
