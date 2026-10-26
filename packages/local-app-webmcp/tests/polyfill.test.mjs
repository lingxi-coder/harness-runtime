import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { test } from 'node:test';
import { runInNewContext } from 'node:vm';

const bundle = await readFile(new URL('../dist/webmcp-polyfill.iife.js', import.meta.url), 'utf8');

test('auto entry preserves an existing native document.modelContext', () => {
  const nativeContext = Object.freeze({ native: true });
  const document = { modelContext: nativeContext };
  const realm = {
    document,
    isSecureContext: true,
    DOMException,
    EventTarget,
  };

  runInNewContext(bundle, realm);

  assert.strictEqual(document.modelContext, nativeContext);
});
