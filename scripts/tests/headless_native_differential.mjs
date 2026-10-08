#!/usr/bin/env node
// Capture first, compare second. Passing includes all requested raw channels;
// semantic smoke assertions are observations and cannot declare byte parity.
import {mkdtemp, mkdir, readFile, readdir, rm, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {fixtures, schemaProbes, lifecycleProbes, validateFixtures} from './headless-fixtures/matrix.mjs';
import {startProvider} from './headless-fixtures/loopback-provider.mjs';
import {captureProcess, sha256} from './headless-fixtures/capture.mjs';
import {compareBuffers, createComparisonContext} from './headless-byte-compare.mjs';
import {compareMetadataWrapped, compareHttpWrapped, resolvePidSocketRules, mapForkSessionFiles, compareForkSessionHeaders} from './headless-fixtures/normalization-adapter.mjs';

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const baseline = JSON.parse(await readFile(path.join(repo, 'docs/parity/headless-native-baseline.json')));
const options = {only: undefined, timeoutMs: 15000, providerDelayMs: 0, startupSettleMs: 0};
const args = process.argv.slice(2);
for (let index = 0; index < args.length; index++) {
  const flag = args[index];
  if (!['--execute', '--compare-existing', '--schema-probes', '--lifecycle-probes', '--native-once', '--text-stdin', '--startup-settle-ms', '--harness-config', '--only', '--output', '--rules', '--timeout-ms', '--provider-delay-ms', '--help'].includes(flag)) throw new Error(`Unknown option ${flag}`);
  if (flag === '--help') options.help = true;
  else if (flag === '--schema-probes') options.schemaProbes = true;
  else if (flag === '--lifecycle-probes') options.lifecycleProbes = true;
  else if (flag === '--native-once') options.nativeOnce = true;
  else if (flag === '--text-stdin') options.textStdin = true;
  else {
    const value = args[++index];
    if (!value || value.startsWith('--')) throw new Error(`Missing value for ${flag}`);
    options[{'--execute': 'native', '--compare-existing': 'existing', '--harness-config': 'harnessConfig', '--only': 'only', '--output': 'output', '--rules': 'rules', '--timeout-ms': 'timeoutMs', '--provider-delay-ms': 'providerDelayMs', '--startup-settle-ms': 'startupSettleMs'}[flag]] = ['--timeout-ms', '--provider-delay-ms', '--startup-settle-ms'].includes(flag) ? Number(value) : value;
  }
}
if (!Number.isInteger(options.timeoutMs) || options.timeoutMs < 1000 || options.timeoutMs > 60000) throw new Error('--timeout-ms must be 1000..60000');
if (!Number.isInteger(options.providerDelayMs) || options.providerDelayMs < 0 || options.providerDelayMs > 2000) throw new Error('--provider-delay-ms must be 0..2000');
if (!Number.isInteger(options.startupSettleMs) || options.startupSettleMs < 0 || options.startupSettleMs > 2000) throw new Error('--startup-settle-ms must be 0..2000');
const ids = options.only?.split(',');
const existingReport = options.existing ? JSON.parse(await readFile(path.join(path.resolve(options.existing), 'report.json'))) : undefined;
if (existingReport && existingReport.baseline.binary.sha256 !== baseline.binary.sha256) throw new Error('Existing report baseline does not match pinned native');
if (existingReport && (options.native || options.harnessConfig || options.output)) throw new Error('--compare-existing only reads existing captures; cannot launch engines or overwrite output');
if (options.schemaProbes && options.lifecycleProbes) throw new Error('Select one research matrix');
if (options.nativeOnce && (!options.lifecycleProbes || options.harnessConfig || options.existing)) throw new Error('--native-once is single-native lifecycle research only');
const activeFixtures = options.lifecycleProbes ? lifecycleProbes : options.schemaProbes ? schemaProbes : fixtures;
const selected = validateFixtures(activeFixtures.filter(fixture => (!ids || ids.includes(fixture.id)) && (!existingReport || existingReport.fixtureIds.includes(fixture.id))).map(fixture => ({...fixture, providerDelayMs: options.providerDelayMs || fixture.providerDelayMs || 0, startupSettleMs: options.startupSettleMs, ...options.textStdin ? {textStdin: true} : {}})));
if (options.textStdin && selected.some(fixture => fixture.input === 'stream-json')) throw new Error('--text-stdin cannot be combined with SDK fixtures');
if (selected.length === 0 || ids?.some(id => !selected.some(fixture => fixture.id === id))) throw new Error('Unknown or empty --only fixture selection');
if (options.help || !options.native && !options.existing) {
  console.log(JSON.stringify({mode: 'review-only', native_invoked: false, baseline, fixtures: selected.map(f => f.id), invocation: 'node scripts/tests/headless_native_differential.mjs --execute <pinned-native> [--harness-config <json>] [--only id,id] [--rules <json>] [--output <new-directory>]', harnessConfig: {command: '/absolute/path/to/harness-host', args: [], flags: [], env: {LINGXI_DATA_DIR: '{{state}}'}}, rules: {version: 1, nativeNative: {'fixture-id': {'processes/0/stdout.bin': []}}, nativeHarness: {'fixture-id': {'processes/0/stdout.bin': []}}}, note: 'No default normalization. Native-native runs precede all Native-Harness runs. Every raw byte is retained. A missing Harness host or unresolved native nondeterminism prevents a full parity verdict.'}, null, 2));
  process.exit(0);
}
let native;
if (!options.existing) {
  const nativeBinary = path.resolve(options.native);
  if (`${process.platform}-${process.arch}` !== baseline.platform) throw new Error(`Pinned native baseline requires ${baseline.platform}`);
  if (sha256(await readFile(nativeBinary)) !== baseline.binary.sha256) throw new Error(`Native binary must match the pinned ${baseline.version}/${baseline.platform} SHA256`);
  native = {command: nativeBinary, flags: ['--bare']};
}
let harness;
if (options.harnessConfig) {
  harness = JSON.parse(await readFile(path.resolve(options.harnessConfig)));
  if (!harness.command || !path.isAbsolute(harness.command)) throw new Error('Harness command must be an absolute executable path');
  if (![harness.args ?? [], harness.flags ?? []].every(value => Array.isArray(value) && value.every(item => typeof item === 'string'))) throw new Error('Harness args/flags must be string arrays');
  if (harness.env && (typeof harness.env !== 'object' || Array.isArray(harness.env))) throw new Error('Harness env must be an object');
  harness.flags = ['--bare', ...harness.flags ?? []].filter((flag, index, values) => flag !== '--bare' || values.indexOf(flag) === index);
}
const rules = options.rules ? JSON.parse(await readFile(path.resolve(options.rules))) : {version: 1};
if (rules.version !== 1) throw new Error('Rules version must equal 1');
for (const key of Object.keys(rules)) if (!['version', 'nativeNative', 'nativeHarness', 'evidence', 'unresolved'].includes(key)) throw new Error(`Unknown rules field ${key}`);
const root = options.existing ? path.resolve(options.existing) : options.output ? path.resolve(options.output) : await mkdtemp(path.join(tmpdir(), 'headless-native-differential-'));
if (options.output) await mkdir(root); // Never overwrite an existing evidence directory.
const provider = options.existing ? undefined : await startProvider();
const report = {schemaVersion: 1, baseline, root, comparisonOnly: Boolean(options.existing), protocolContext: existingReport?.protocolContext ?? {schemaResearch: Boolean(options.schemaProbes), lifecycleResearch: Boolean(options.lifecycleProbes), nativeRepetitions: options.nativeOnce ? 1 : 2, startupSettleMs: options.startupSettleMs, textStdin: Boolean(options.textStdin), providerDelayMs: options.providerDelayMs}, provider: provider?.baseUrl ?? existingReport?.provider, inheritedEnvironment: false, credentialMode: 'fixed dummy only', nativeNativeCompletedBeforeHarness: false, fixtureIds: selected.map(f => f.id), captures: existingReport?.captures ?? {}, nativeNative: [], nativeHarness: [], scope: 'Raw stdin/stdout/stderr/exit, exact local HTTP request/response wire, and this fixture session JSONL. Native result is observed before EOF/drain. No raw data is canonicalized or discarded.'};

async function run(fixture, engine, label) {
  const fixtureRoot = path.join(root, fixture.id);
  const locations = {workspace: path.join(fixtureRoot, 'workspace'), state: path.join(fixtureRoot, 'state'), tmp: path.join(fixtureRoot, 'tmp')};
  // All these paths were allocated beneath this run's fresh evidence root.
  // Reuse identical paths between native repetitions so path variation does
  // not require a broad output replacement; remove only our transient state.
  for (const dir of Object.values(locations)) { await rm(dir, {recursive: true, force: true}); await mkdir(dir, {recursive: true}); }
  const captureDir = path.join(fixtureRoot, label);
  const state = provider.begin(fixture, captureDir);
  const observations = [];
  observations.push(await captureProcess({engine, fixture, captureDir: path.join(captureDir, 'processes/0'), locations, provider, providerState: state, timeoutMs: options.timeoutMs}));
  await provider.settle();
  if (fixture.resume && !observations[0].timedOut && observations[0].exit.code === 0) {
    state.processIndex = 1;
    observations.push(await captureProcess({engine, fixture, captureDir: path.join(captureDir, 'processes/1'), locations, provider, providerState: state, resume: true, timeoutMs: options.timeoutMs}));
    await provider.settle();
  }
  const validation = observations.every((observation, index) => !observation.timedOut && !observation.exit.spawnError && (fixture.probe ? [0, 1].includes(observation.exit.code) : observation.exit.code === (fixture.expectedExit ?? (fixture.malformed ? 1 : 0))) && (fixture.format === 'text' || fixture.malformed || (index === 0 && fixture.acceptedResultCounts ? fixture.acceptedResultCounts.includes(observation.resultCount) : observation.resultCount === (index === 1 ? 1 : fixture.expectedResultCount ?? fixture.turns.length))) && (!fixture.expectedSubtype || observation.resultSubtypes.includes(fixture.expectedSubtype)) && (!fixture.approval || fixture.requirePermissionRequest === false || observation.permissionRequests > 0) && observation.controlResponses.length === (fixture.controls?.length ?? 0) + (fixture.afterResultControls?.length ?? 0) && (fixture.persistence === false ? observation.sessions.length === 0 : true)) && state.unexpected.length === 0;
  const receipt = {fixture: fixture.id, label, captureDir, observations, provider: {requestCount: state.requests.length, messageCount: state.messageCount, unexpected: state.unexpected, socketErrors: state.socketErrors}, validation, validationMeaning: 'Capture/protocol completion only. This boolean is never a byte-parity result.'};
  await writeFile(path.join(captureDir, 'receipt.json'), JSON.stringify(receipt, null, 2) + '\n');
  report.captures[`${fixture.id}/${label}`] = receipt;
  console.error(`${fixture.id}/${label}: exit=${observations.map(row => row.exit.code).join(',')} results=${observations.map(row => row.resultCount ?? 'n/a').join(',')} messages=${state.messageCount} capture=${validation ? 'complete' : 'incomplete'}`);
}

async function artifacts(dir) {
  const files = new Map();
  async function walk(at, relative = '') {
    let entries; try { entries = await readdir(at, {withFileTypes: true}); } catch (error) { if (error.code === 'ENOENT') return; throw error; }
    for (const entry of entries.sort((a, b) => a.name.localeCompare(b.name))) {
      const name = path.join(relative, entry.name); const full = path.join(at, entry.name);
      if (entry.isDirectory()) await walk(full, name);
      else if (entry.isFile() && (name.endsWith('.bin') || name.endsWith('.jsonl') && name.includes(`${path.sep}sessions${path.sep}`) || name.endsWith(`${path.sep}exit.json`))) files.set(name, await readFile(full));
    }
  }
  await walk(dir);
  return files;
}
function formatFor(fixture, artifact, leftBytes, rightBytes) {
  if (artifact.endsWith('exit.json') || artifact.endsWith('request-body.bin') && leftBytes.length > 0 && rightBytes.length > 0) return 'json';
  if (artifact.includes(`${path.sep}sessions${path.sep}`)) return 'ndjson';
  if (artifact.endsWith('stdout.bin')) return fixture.format === 'json' ? 'json' : fixture.format === 'stream-json' ? 'ndjson' : 'raw';
  if (artifact.endsWith('stdin.bin') && fixture.input === 'stream-json' && !fixture.malformed) return 'ndjson';
  return 'raw';
}
async function compare(fixture, leftLabel, rightLabel, phase) {
  let left = await artifacts(path.join(root, fixture.id, leftLabel));
  let right = await artifacts(path.join(root, fixture.id, rightLabel));
  const context = createComparisonContext();
  const entries = [];
  const fixtureRules = rules[phase]?.[fixture.id] ?? {};
  let filenameIdentity;
  if (fixtureRules['@session-files']) {
    if (!fixture.resumeFork) throw new Error('Fork filename rules require an explicit fork fixture');
    const mapped = mapForkSessionFiles(left, right, {context, declaration: fixtureRules['@session-files'], leftId: report.captures[`${fixture.id}/${leftLabel}`].observations[1]?.primarySessionId, rightId: report.captures[`${fixture.id}/${rightLabel}`].observations[1]?.primarySessionId});
    left = mapped.left.files; right = mapped.right.files; filenameIdentity = mapped.mapping;
  }
  for (const artifact of new Set([...left.keys(), ...right.keys()])) {
    if (!left.has(artifact) || !right.has(artifact)) { entries.push({artifact, equal: false, kind: 'missing-artifact', leftPresent: left.has(artifact), rightPresent: right.has(artifact)}); continue; }
    const leftBytes = left.get(artifact), rightBytes = right.get(artifact);
    try {
      const sessionAlias = artifact.includes('/sessions/') ? path.basename(artifact) === '@forked-session.jsonl' ? artifact.replace(/\/sessions\/.*$/, '/sessions/@forked-session.jsonl') : path.basename(artifact) === `${report.captures[`${fixture.id}/${leftLabel}`].observations[Number(artifact.split('/')[1])].sessionId}.jsonl` ? artifact.replace(/\/sessions\/.*$/, '/sessions/@fixture-session.jsonl') : undefined : undefined;
      const ruleEntry = fixtureRules[artifact] ?? (sessionAlias ? fixtureRules[sessionAlias] : undefined) ?? [];
      let comparison;
      if (Array.isArray(ruleEntry)) {
        const format = formatFor(fixture, artifact, leftBytes, rightBytes);
        const processIndex = Number(artifact.split('/')[1]);
        const pid = (label, bytes) => {
          const saved = report.captures[`${fixture.id}/${label}`].observations[processIndex]?.pid;
          if (saved !== undefined) return saved;
          if (!ruleEntry.some(rule => rule.kind === 'native-pid-socket')) return undefined;
          const rows = bytes.toString().trim().split('\n').map(JSON.parse);
          const responses = rows.filter(row => row.type === 'control_response' && row.response?.request_id === 'fixture-initialize');
          if (responses.length !== 1) throw new Error('Missing exact initialization response PID');
          return responses[0].response.response.pid;
        };
        const resolvedRules = resolvePidSocketRules(ruleEntry, {left: leftBytes, right: rightBytes, format, leftPid: pid(leftLabel, leftBytes), rightPid: pid(rightLabel, rightBytes)});
        comparison = compareBuffers(leftBytes, rightBytes, {format, channel: artifact, rules: resolvedRules, context});
      }
      else if (Object.keys(ruleEntry).some(key => !['wrapper', 'metadata', 'header'].includes(key))) throw new Error(`Unknown wrapper rule property ${artifact}`);
      else if (ruleEntry.wrapper === 'metadata-json-string' && artifact.endsWith('request-body.bin')) comparison = compareMetadataWrapped(leftBytes, rightBytes, {format: 'json', channel: artifact, context, metadata: ruleEntry.metadata});
      else if (ruleEntry.wrapper === 'http-request-metadata' && artifact.endsWith('.request.bin')) comparison = compareHttpWrapped(leftBytes, rightBytes, {channel: artifact, context, metadata: ruleEntry.metadata, header: ruleEntry.header});
      else if (ruleEntry.wrapper === 'fork-session-header' && artifact.endsWith('.request-headers.bin')) comparison = compareForkSessionHeaders(leftBytes, rightBytes, {context, declaration: ruleEntry.header});
      else throw new Error(`Invalid wrapper target ${artifact}`);
      entries.push({artifact, equal: comparison.equal, rawEqual: comparison.rawEqual, normalizedEqual: comparison.normalizedEqual, transformations: comparison.transformations, difference: comparison.difference, left: {bytes: leftBytes.length, sha256: sha256(leftBytes)}, right: {bytes: rightBytes.length, sha256: sha256(rightBytes)}});
    } catch (error) { entries.push({artifact, equal: false, kind: 'comparison-rejected', error: String(error), details: error.details}); }
  }
  for (const artifact of Object.keys(fixtureRules)) {
    if (artifact === '@session-files') continue;
    if (artifact.endsWith('/sessions/@fixture-session.jsonl')) {
      const prefix = artifact.slice(0, -'@fixture-session.jsonl'.length);
      const id = report.captures[`${fixture.id}/${leftLabel}`].observations[Number(artifact.split('/')[1])].sessionId;
      for (const [side, files] of [['left', left], ['right', right]]) if ([...files.keys()].filter(name => name.startsWith(prefix) && path.basename(name) === `${id}.jsonl`).length !== 1) entries.push({artifact, equal: false, kind: 'session-rule-target-cardinality', side});
    } else if (artifact.endsWith('/sessions/@forked-session.jsonl')) {
      const prefix = artifact.slice(0, -'@forked-session.jsonl'.length);
      for (const [side, files] of [['left', left], ['right', right]]) if ([...files.keys()].filter(name => name.startsWith(prefix) && path.basename(name) === '@forked-session.jsonl').length !== 1) entries.push({artifact, equal: false, kind: 'fork-session-rule-target-cardinality', side});
    } else if (!left.has(artifact) || !right.has(artifact)) entries.push({artifact, equal: false, kind: 'rule-target-missing'});
  }
  return {fixture: fixture.id, left: leftLabel, right: rightLabel, filenameIdentity, equal: entries.length > 0 && entries.every(entry => entry.equal), entries};
}

try {
  // Complete the two native captures and native-native byte comparisons for
  // every selected fixture before any Harness process is launched.
  for (const fixture of selected) {
    if (!options.existing) { await run(fixture, native, 'native-a'); if (!options.nativeOnce) await run(fixture, native, 'native-b'); }
    report.nativeNative.push(options.nativeOnce ? {fixture: fixture.id, equal: false, notRun: true, reason: 'Explicit single-native research; no native-native stability evidence'} : await compare(fixture, 'native-a', 'native-b', 'nativeNative'));
  }
  report.nativeNativeCompletedBeforeHarness = !options.nativeOnce;
  if (!options.existing) await writeFile(path.join(root, 'native-native-report.json'), JSON.stringify({baseline, comparisons: report.nativeNative}, null, 2) + '\n');
  if (harness) for (const fixture of selected) {
    await run(fixture, harness, 'harness');
    const comparison = await compare(fixture, 'native-a', 'harness', 'nativeHarness');
    comparison.nativeStable = report.nativeNative.find(item => item.fixture === fixture.id).equal;
    comparison.parityEstablished = comparison.equal && comparison.nativeStable && report.captures[`${fixture.id}/native-a`].validation && report.captures[`${fixture.id}/native-b`].validation && report.captures[`${fixture.id}/harness`].validation;
    report.nativeHarness.push(comparison);
  }
} finally {
  if (provider) await provider.close();
  // Raw captures survive; only our reproducible workspace/config/tmp are
  // removed. Never remove another runner's root or a user cache/binary.
  if (!options.existing) for (const fixture of selected) for (const name of ['workspace', 'state', 'tmp']) await rm(path.join(root, fixture.id, name), {recursive: true, force: true});
}
report.verdict = {
  nativeCapturesComplete: Object.entries(report.captures).filter(([key]) => key.includes('/native-')).every(([, receipt]) => receipt.validation),
  nativeStable: report.nativeNative.length === selected.length && report.nativeNative.every(comparison => comparison.equal),
  harnessInvoked: Boolean(harness) || Boolean(existingReport?.verdict.harnessInvoked),
  fullMatrixSelected: !options.schemaProbes && selected.length === fixtures.length,
  parityEstablished: !options.schemaProbes && Boolean(harness) && selected.length === fixtures.length && report.nativeHarness.length === selected.length && report.nativeHarness.every(comparison => comparison.parityEstablished),
};
if (!options.existing) await writeFile(path.join(root, 'report.json'), JSON.stringify(report, null, 2) + '\n');
if (options.existing) console.log(JSON.stringify(report, null, 2));
else console.log(JSON.stringify({root, report: path.join(root, 'report.json'), verdict: report.verdict}, null, 2));
process.exitCode = options.existing ? report.verdict.nativeStable ? 0 : 1 : harness ? report.verdict.parityEstablished ? 0 : 1 : report.verdict.nativeCapturesComplete ? 0 : 1;
