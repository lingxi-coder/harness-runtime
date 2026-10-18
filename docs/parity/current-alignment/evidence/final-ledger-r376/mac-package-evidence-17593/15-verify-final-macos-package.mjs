#!/usr/bin/env node

import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { readFileSync, realpathSync, statSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { packageRoot } from '../../apps/electron/scripts/package-support.mjs';
import { verifyPackage } from '../../apps/electron/scripts/verify-package.mjs';

function fail(message) {
  throw new Error(message);
}

function sha256(path) {
  return createHash('sha256').update(readFileSync(path)).digest('hex');
}

const [rawAppPath, productSha, harnessSha] = process.argv.slice(2);
const pinnedBun = {
  version: '1.4.3',
  revision: 'c6da4a4d3010e5553438c60f6bd76d981976867c',
  sourceSha256: '28c7dd0a06916aba284d570ef44a2d70e9c538eaf2ba58dfe0910525563b3f79',
};
if (!rawAppPath || !/^[a-f0-9]{40}$/.test(productSha ?? '') || !/^[a-f0-9]{40}$/.test(harnessSha ?? '')) {
  fail('usage: verify-final-macos-package.mjs <appPath> <productSha40> <harnessSha40>');
}

const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const electronRoot = join(repositoryRoot, 'apps', 'electron');
const appPath = realpathSync(resolve(rawAppPath));
if (!statSync(appPath).isDirectory() || !appPath.endsWith('/LingXi Code.app')) {
  fail(`appPath must resolve to a LingXi Code.app bundle: ${appPath}`);
}

const actualProductSha = execFileSync('git', ['-C', repositoryRoot, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
if (actualProductSha !== productSha) {
  fail(`Product source SHA mismatch: expected ${productSha}, found ${actualProductSha}`);
}

const relevantChanges = execFileSync('git', [
  '-C', repositoryRoot,
  'status', '--porcelain', '--untracked-files=all', '--',
  'Cargo.toml', 'Cargo.lock', 'build.rs', '.cargo', 'apps', 'crates', 'scripts',
], { encoding: 'utf8' }).trim();
if (relevantChanges) fail(`packaged Product inputs are dirty:\n${relevantChanges}`);

const lockText = readFileSync(join(repositoryRoot, 'Cargo.lock'), 'utf8');
const harnessRevisions = [...lockText.matchAll(/source = "git\+[^"\n]*harness-runtime\.git[^"\n]*#([a-f0-9]{40})"/g)]
  .map((match) => match[1]);
if (harnessRevisions.length === 0 || new Set(harnessRevisions).size !== 1 || harnessRevisions[0] !== harnessSha) {
  fail(`Cargo.lock Harness revision mismatch: expected only ${harnessSha}, found ${[...new Set(harnessRevisions)].join(', ') || 'none'}`);
}

const teamId = process.env.LINGXI_MAC_TEAM_ID;
if (teamId !== 'AZ4AX7J833') fail('LINGXI_MAC_TEAM_ID must be AZ4AX7J833 for strict team/entitlement verification');

const outputRoot = realpathSync(resolve(dirname(dirname(appPath))));
const result = verifyPackage(electronRoot, outputRoot);
if (realpathSync(result.appPath) !== appPath) {
  fail(`appPath does not match the package verifier's artifact path: ${result.appPath}`);
}

const resources = join(appPath, 'Contents', 'Resources');
const asarPath = join(resources, 'app.asar');
const bunManifest = JSON.parse(readFileSync(join(resources, 'mod-bun.json'), 'utf8'));
if (bunManifest.version !== pinnedBun.version
  || bunManifest.revision !== pinnedBun.revision
  || bunManifest.sourceSha256 !== pinnedBun.sourceSha256) {
  fail(`packaged Bun does not match the frozen 1.4.3 source pin: ${JSON.stringify({
    version: bunManifest.version,
    revision: bunManifest.revision,
    sourceSha256: bunManifest.sourceSha256,
  })}`);
}
process.stdout.write(`[mac-package-audit] Product ${productSha}\n`);
process.stdout.write(`[mac-package-audit] Harness Cargo.lock ${harnessSha}\n`);
process.stdout.write(`[mac-package-audit] app.asar SHA-256 ${sha256(asarPath)}\n`);
process.stdout.write(`[mac-package-audit] Bun ${bunManifest.version} revision ${bunManifest.revision}; source SHA-256 ${bunManifest.sourceSha256}; sealed SHA-256 ${bunManifest.sha256}\n`);
process.stdout.write(`[mac-package-audit] static package verification passed: ${result.appPath}\n`);
