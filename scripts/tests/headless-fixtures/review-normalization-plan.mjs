#!/usr/bin/env node
// Offline review aid. The resulting plan is frozen before comparison; the
// differential runner never learns or widens its rules from a new capture.
import {readFile} from 'node:fs/promises';
import path from 'node:path';
import {sha256} from './capture.mjs';
import {jsonScalarSpans} from './normalization-adapter.mjs';
import {fixtures} from './matrix.mjs';

const uuid = /^[a-f0-9]{8}-[a-f0-9]{4}-[1-8][a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/i;
const timingPaths = new Set(['/duration_api_ms', '/duration_ms', '/ttft_ms', '/ttft_stream_ms', '/time_to_request_ms', '/first_content_frame_ms', '/response/response/pid', '/totalAPIDuration', '/totalAPIDurationWithoutRetries', '/totalDuration', '/startTime', '/totalToolDuration', '/attachment/durationMs']);
const identityPaths = new Set(['/uuid', '/parentUuid', '/leafUuid', '/promptId', '/sourceToolAssistantUUID', '/request_id', '/response/request_id', '/hook_id', '/attachment/toolUseID', '/sessionId', '/session_id']);
const wrapper = kind => ({wrapper: kind, metadata: {id: 'metadata-device', outerPath: '/metadata/user_id', innerPath: '/device_id', count: 1, group: 'device'}});

export async function reviewPlan(roots) {
  const plan = {version: 1, evidence: {baseline: '2.1.293', reviewRequired: true, sourceReports: [], policy: 'Exact per-fixture artifact paths/counts; only enumerated UUID, ISO-time, timing, PID and nested metadata device scalar paths. No sorting, record omission, whole-string metadata replacement or automatic runtime discovery.'}, nativeNative: {}, nativeHarness: {}, unresolved: []};
  for (const root of roots) {
    const reportPath = path.join(path.resolve(root), 'report.json');
    const raw = await readFile(reportPath); const report = JSON.parse(raw);
    if (report.baseline.version !== '2.1.293' || report.baseline.binary.sha256 !== '4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e') throw new Error('Unexpected source native baseline');
    plan.evidence.sourceReports.push({path: reportPath, sha256: sha256(raw)});
    for (const comparison of report.nativeNative) {
      const fixture = fixtures.find(row => row.id === comparison.fixture);
      if (!fixture) throw new Error(`Unknown fixture ${comparison.fixture}`);
      const artifacts = {};
      if (fixture.resumeFork) artifacts['@session-files'] = {kind: 'fork-init', process: 1, count: 1, group: 'session'};
      for (const entry of comparison.entries) {
        if (entry.leftPresent === false) continue;
        const artifact = entry.artifact;
        const bytes = await readFile(path.join(report.root, fixture.id, 'native-a', artifact));
        if (artifact.endsWith('request-body.bin') && bytes.length && jsonScalarSpans(bytes).some(row => row.path === '/metadata/user_id')) {
          const rule = wrapper('metadata-json-string');
          if (fixture.resumeFork && JSON.parse(await readFile(path.join(report.root, fixture.id, 'native-a', artifact.replace('.request-body.bin', '.meta.json')))).process === 1) rule.metadata.normalizeForkSession = true;
          artifacts[artifact] = rule; continue;
        }
        if (artifact.endsWith('.request.bin') && artifacts[artifact.replace('.request.bin', '.request-body.bin')]) { const rule = wrapper('http-request-metadata'); rule.metadata = structuredClone(artifacts[artifact.replace('.request.bin', '.request-body.bin')].metadata); if (rule.metadata.normalizeForkSession) rule.header = {name: 'x-claude-code-session-id', count: 1, group: 'session'}; artifacts[artifact] = rule; continue; }
        if (artifact.endsWith('.request-headers.bin') && artifacts[artifact.replace('.request-headers.bin', '.request-body.bin')]?.metadata.normalizeForkSession) { artifacts[artifact] = {wrapper: 'fork-session-header', header: {name: 'x-claude-code-session-id', count: 1, group: 'session'}}; continue; }
        const session = artifact.includes('/sessions/');
        if (!session && !artifact.endsWith('stdout.bin') && !artifact.endsWith('stdin.bin')) continue;
        if (artifact.endsWith('stdout.bin') && fixture.format === 'text' || artifact.endsWith('stdin.bin') && fixture.input !== 'stream-json') continue;
        const format = session || artifact.endsWith('stdin.bin') || fixture.format === 'stream-json' ? 'ndjson' : 'json';
        let spans, rows;
        try { spans = jsonScalarSpans(bytes, format); rows = format === 'ndjson' ? bytes.toString().trim().split('\n').map(JSON.parse) : [JSON.parse(bytes)]; }
        catch { continue; } // Invalid fixture bytes get no normalization.
        const grouped = new Map();
        for (const span of spans) {
          const field = span.path.replace(/^\/\d+(?=\/)/, '');
          if (!timingPaths.has(field) && !identityPaths.has(field) && field !== '/timestamp') continue;
          if (['/sessionId', '/session_id'].includes(field) && (!fixture.resumeFork || span.value === report.captures[`${fixture.id}/native-a`].observations[Number(artifact.split('/')[1])]?.sessionId)) continue;
          if (identityPaths.has(field) && (span.type !== 'string' || !uuid.test(span.value))) continue;
          if (timingPaths.has(field) && span.type !== 'number' || field === '/timestamp' && span.type !== 'string') continue;
          const recordType = format === 'json' && Array.isArray(rows[0]) ? rows[0][Number(span.path.split('/')[1])]?.type : rows[span.record]?.type;
          const match = format === 'ndjson' && typeof recordType === 'string' ? {path: '/type', equals: recordType} : undefined;
          const key = JSON.stringify([span.path, recordType, match]);
          if (!grouped.has(key)) grouped.set(key, {path: span.path, type: span.type, field, recordType, match, selected: []});
          grouped.get(key).selected.push(span);
        }
        const rules = [...grouped.values()].map((item, index) => {
          const rule = {id: `dynamic-${index}-${item.field.slice(1).replaceAll('/', '-')}-${item.recordType ?? 'root'}`, path: item.path, type: item.type, count: item.selected.length, ...item.match ? {match: item.match} : {}};
          if (identityPaths.has(item.field)) {
            rule.mode = 'identity'; rule.validate = 'uuid';
            rule.group = ['/sessionId', '/session_id'].includes(item.field) ? 'session' : ['/request_id', '/response/request_id'].includes(item.field) ? 'permission' : item.field === '/hook_id' ? 'hook' : item.field === '/attachment/toolUseID' ? 'hook-tool' : item.field === '/promptId' ? 'prompt' : ['/parentUuid', '/leafUuid', '/sourceToolAssistantUUID'].includes(item.field) || ['user', 'assistant', 'attachment'].includes(item.recordType) ? 'message' : `${item.recordType ?? 'result'}-uuid`;
            const eligible = spans.filter(span => span.path === item.path && (!item.match || rows[span.record]?.type === item.match.equals));
            if (eligible.length !== item.selected.length) rule.records = item.selected.map(span => span.record);
          } else { rule.mode = 'value'; rule.validate = item.field === '/timestamp' ? 'iso-timestamp' : item.field === '/response/response/pid' ? 'integer' : 'nonnegative-number'; }
          return rule;
        });
        for (const span of spans.filter(row => /^\/(?:\d+\/)?messaging_socket_path$/.test(row.path))) rules.push({id: 'native-process-socket', kind: 'native-pid-socket', path: span.path, count: 1});
        const forkSession = fixture.resumeFork && Number(artifact.split('/')[1]) === 1 && path.basename(artifact) === `${report.captures[`${fixture.id}/native-a`].observations[1].primarySessionId}.jsonl`;
        if (rules.length) artifacts[session ? artifact.replace(/\/sessions\/.*$/, forkSession ? '/sessions/@forked-session.jsonl' : '/sessions/@fixture-session.jsonl') : artifact] = rules;
      }
      plan.nativeNative[fixture.id] = artifacts;
      plan.nativeHarness[fixture.id] = structuredClone(artifacts);
    }
  }
  return plan;
}

if (process.argv[1] && path.resolve(process.argv[1]) === new URL(import.meta.url).pathname) {
  const roots = process.argv.slice(2);
  if (!roots.length) throw new Error('Usage: review-normalization-plan.mjs <existing-evidence-root>... > <candidate.json>');
  console.log(JSON.stringify(await reviewPlan(roots), null, 2));
}
