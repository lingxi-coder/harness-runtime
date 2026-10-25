import {createHash} from 'node:crypto';
import {copyFile, mkdir, readFile, readdir, writeFile} from 'node:fs/promises';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {build} from 'esbuild';

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const dist = path.join(packageRoot, 'dist');
const types = path.join(dist, 'types');
const licenses = path.join(dist, 'licenses');
await mkdir(types, {recursive: true});
await mkdir(licenses, {recursive: true});

const shared = {
  absWorkingDir: packageRoot,
  bundle: true,
  charset: 'utf8',
  legalComments: 'eof',
  minify: true,
  platform: 'browser',
  sourcemap: false,
  target: ['es2022'],
  treeShaking: true,
  write: true,
  logLevel: 'warning',
};

await build({...shared, entryPoints: ['src/index.js'], outfile: 'dist/client.bundle.js', format: 'esm'});
await build({...shared, entryPoints: ['src/browser.js'], outfile: 'dist/browser.entry.js', format: 'esm'});
await build({
  ...shared,
  entryPoints: ['src/browser.js'],
  outfile: 'dist/browser.global.js',
  format: 'iife',
  globalName: 'LingxiLocalAppAGUI',
});
const browserGlobalSource = await readFile(path.join(dist, 'browser.global.js'), 'utf8');
await writeFile(path.join(dist, 'browser.source.js'),
  `export const browserAgentSource = ${JSON.stringify(browserGlobalSource)};\nexport default browserAgentSource;\n`);

for (const name of ['index.d.ts', 'client.d.ts', 'browser.d.ts', 'browser-source.d.ts', 'file.d.ts', 'validators.d.ts']) {
  await copyFile(path.join(packageRoot, 'src', name), path.join(types, name));
}
await copyFile(path.join(packageRoot, 'licenses/AG-UI-MIT.txt'), path.join(licenses, 'AG-UI-MIT.txt'));
await copyFile(path.join(packageRoot, 'licenses/README.md'), path.join(licenses, 'README.md'));

const lock = JSON.parse(await readFile(path.join(packageRoot, 'package-lock.json'), 'utf8'));
const thirdPartyRows = [];
const thirdPartyDir = path.join(licenses, 'third-party');
await mkdir(thirdPartyDir, {recursive: true});
for (const [installPath, lockedPackage] of Object.entries(lock.packages || {})) {
  if (!installPath.startsWith('node_modules/') || lockedPackage.dev || lockedPackage.optional) continue;
  const installedPath = path.join(packageRoot, installPath);
  let dependency;
  try {
    dependency = JSON.parse(await readFile(path.join(installedPath, 'package.json'), 'utf8'));
  } catch {
    continue;
  }
  const licenseValue = typeof dependency.license === 'string'
    ? dependency.license
    : dependency.license?.type || dependency.licenses?.map(item => item.type).join(', ') || 'unspecified';
  thirdPartyRows.push({name: dependency.name, version: dependency.version, license: lockedPackage.license || licenseValue});
  const packageSlug = `${dependency.name.replace(/^@/, '').replaceAll('/', '-')}-${dependency.version}`;
  for (const licenseFile of ['LICENSE', 'LICENSE.md', 'LICENSE.txt', 'COPYING', 'NOTICE', 'NOTICE.md']) {
    try {
      await copyFile(path.join(installedPath, licenseFile), path.join(thirdPartyDir, `${packageSlug}-${licenseFile}`));
    } catch {
      // Some packages publish their license in metadata without a standalone file.
    }
  }
}
thirdPartyRows.sort((a, b) => `${a.name}@${a.version}`.localeCompare(`${b.name}@${b.version}`));
const notices = [
  '# Third-party notices',
  '',
  'This generated bundle includes the following locked npm packages. Upstream license and notice files are copied under `third-party/` when they are published separately.',
  '',
  '| Package | Version | License |',
  '| --- | --- | --- |',
  ...thirdPartyRows.map(row => `| ${row.name} | ${row.version} | ${row.license} |`),
  '',
].join('\n');
await writeFile(path.join(licenses, 'THIRD-PARTY-NOTICES.md'), notices);

const provenance = JSON.parse(await readFile(path.join(packageRoot, 'spec/provenance.json'), 'utf8'));
const schemaPath = path.join(packageRoot, 'spec', provenance.schemaFile);
const schemaBytes = await readFile(schemaPath);
const schemaDigest = createHash('sha256').update(schemaBytes).digest('hex');
if (schemaBytes.byteLength !== provenance.schemaBytes || schemaDigest !== provenance.schemaSha256) {
  throw new Error('Vendored AG-UI schema does not match its provenance record.');
}

async function listFiles(directory, prefix = 'dist') {
  const found = [];
  for (const entry of await readdir(directory, {withFileTypes: true})) {
    const absolute = path.join(directory, entry.name);
    const relative = path.posix.join(prefix, entry.name);
    if (relative === 'dist/manifest.json') continue;
    if (entry.isDirectory()) found.push(...await listFiles(absolute, relative));
    else found.push(relative);
  }
  return found;
}

const artifactPaths = (await listFiles(dist)).sort();
const artifacts = {};
for (const relativePath of artifactPaths) {
  const bytes = await readFile(path.join(packageRoot, relativePath));
  artifacts[relativePath] = createHash('sha256').update(bytes).digest('hex');
}

const packageJson = JSON.parse(await readFile(path.join(packageRoot, 'package.json'), 'utf8'));
const npmPackages = {};
for (const name of ['@ag-ui/client', '@ag-ui/core', 'zod']) {
  const locked = lock.packages[`node_modules/${name}`];
  if (!locked) throw new Error(`Missing locked package record for ${name}.`);
  npmPackages[name] = {
    version: locked.version,
    resolved: locked.resolved,
    integrity: locked.integrity,
    license: locked.license,
  };
}
const manifest = {
  schemaVersion: 1,
  package: {name: packageJson.name, version: packageJson.version},
  source: {
    repository: provenance.sourceRepository,
    ref: provenance.sourceRef,
    commit: provenance.sourceCommit,
  },
  dependencies: {
    ...packageJson.dependencies,
    esbuild: packageJson.devDependencies.esbuild,
    typescript: packageJson.devDependencies.typescript,
  },
  npmPackages,
  lockfile: {
    path: 'package-lock.json',
    sha256: createHash('sha256').update(await readFile(path.join(packageRoot, 'package-lock.json'))).digest('hex'),
  },
  schema: {
    path: `spec/${provenance.schemaFile}`,
    sha256: schemaDigest,
    bytes: schemaBytes.byteLength,
  },
  artifacts,
};
await writeFile(path.join(dist, 'manifest.json'), `${JSON.stringify(manifest, null, 2)}\n`);
