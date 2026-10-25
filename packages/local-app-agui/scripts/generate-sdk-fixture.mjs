import {File} from 'node:buffer';
import {mkdir, writeFile} from 'node:fs/promises';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {createLocalAppAgent, fileToContentPart} from '../src/index.js';

if (!globalThis.File) globalThis.File = File;

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const fixtureDirectory = path.join(packageRoot, 'tests', 'fixtures');
const fixturePath = path.join(fixtureDirectory, 'run-agent-input.ag-ui-1.0.3.json');
const photo = await fileToContentPart(new File([new Uint8Array([0x89, 0x50, 0x4e, 0x47])], 'photo.png', {
  type: 'image/png',
}));
photo.id = 'file-fixture-1';

const message = {
  id: 'message-fixture-1',
  role: 'user',
  content: [
    {type: 'text', text: 'Inspect this image.'},
    photo,
  ],
};
const client = createLocalAppAgent({
  transport: {
    start: async input => ({runId: input.runId}),
    events: async () => ({events: [], sequence: 0}),
  },
  threadId: 'thread-fixture-1',
  initialMessages: [message],
});
const input = client.prepareRunAgentInput({
  runId: 'run-fixture-1',
  resume: [],
  forwardedProps: {
    'com.lingxi.localApp': {
      version: 1,
      outputSchema: {
        type: 'object',
        properties: {ok: {type: 'boolean'}},
        required: ['ok'],
        additionalProperties: false,
      },
      tools: ['builtin:Read'],
      skills: ['extract-text'],
      maxTurns: 2,
      timeoutMs: 15000,
    },
  },
});

await mkdir(fixtureDirectory, {recursive: true});
await writeFile(fixturePath, `${JSON.stringify(input, null, 2)}\n`);
