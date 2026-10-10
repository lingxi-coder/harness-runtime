// Writes only in the fresh fixture cwd. Marker timestamps are evidence about
// the real SessionEnd hook, never native stdout protocol events.
import {writeFile} from 'node:fs/promises';
const beginMs = Date.now();
await writeFile('headless-session-end-begin.json', JSON.stringify({beginMs}) + '\n');
await new Promise(resolve => setTimeout(resolve, 200));
await writeFile('headless-session-end.json', JSON.stringify({beginMs, endMs: Date.now()}) + '\n');
