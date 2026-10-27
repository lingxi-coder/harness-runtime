import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { createRequire } from 'node:module';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const upstreamRoot = path.join(packageRoot, 'upstream');
const dist = path.join(packageRoot, 'dist');
const provenance = JSON.parse(await readFile(path.join(packageRoot, 'provenance.json'), 'utf8'));

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex');
}

const sourceFiles = {};
for (const [upstreamPath, expectedHash] of Object.entries(provenance.upstream.sha256)) {
  const pathInPackage = path.join(upstreamRoot, upstreamPath);
  const bytes = await readFile(pathInPackage);
  assert.equal(sha256(bytes), expectedHash, `Pinned WebMCP source changed: ${upstreamPath}`);
  sourceFiles[upstreamPath] = { sha256: expectedHash, bytes: bytes.length };
}

const requireLocal = createRequire(import.meta.url);
let esbuild;
try {
  esbuild = requireLocal('esbuild');
} catch {
  // Reuse the already installed build tool in the sibling AG-UI resource package.
  const requireExisting = createRequire(new URL('../../local-app-agui/package.json', import.meta.url));
  esbuild = requireExisting('esbuild');
}
assert.equal(esbuild.version, provenance.build.esbuildVersion, 'Unexpected esbuild version');

await mkdir(dist, { recursive: true });
const bundlePath = path.join(dist, 'webmcp-polyfill.iife.js');
await esbuild.build({
  absWorkingDir: packageRoot,
  entryPoints: ['upstream/src/auto.ts'],
  outfile: bundlePath,
  bundle: true,
  platform: 'browser',
  format: 'iife',
  target: [provenance.build.target],
  minify: provenance.build.minify,
  legalComments: provenance.build.legalComments,
});

const bundle = await readFile(bundlePath, 'utf8');
assert.match(bundle, /SPDX-License-Identifier: Apache-2\.0/);
const sourceModule = `export const webmcpPolyfillSource = ${JSON.stringify(bundle)};\n`;
await writeFile(path.join(dist, 'webmcp-polyfill.source.js'), sourceModule);
await writeFile(
  path.join(dist, 'webmcp-polyfill.source.d.ts'),
  'export declare const webmcpPolyfillSource: string;\n',
);

const manifest = {
  schemaVersion: 1,
  sourceCommit: provenance.upstream.commit,
  sourceFiles,
  artifact: {
    path: 'webmcp-polyfill.iife.js',
    sha256: sha256(Buffer.from(bundle)),
    bytes: Buffer.byteLength(bundle),
  },
  sourceModule: {
    path: 'webmcp-polyfill.source.js',
    sha256: sha256(Buffer.from(sourceModule)),
    bytes: Buffer.byteLength(sourceModule),
  },
  builder: provenance.build,
};
await writeFile(path.join(dist, 'manifest.json'), `${JSON.stringify(manifest, null, 2)}\n`);
