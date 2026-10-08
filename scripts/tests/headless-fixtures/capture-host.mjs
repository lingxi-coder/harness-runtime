// A protocol fixture for testing the recorder itself. Never a Native parity
// surrogate: it has no model/runtime behavior and is excluded from the matrix.
import {createInterface} from 'node:readline';
const lines = createInterface({input: process.stdin});
let turns = 0;
for await (const line of lines) {
  const row = JSON.parse(line);
  if (row.type === 'control_request') {
    process.stdout.write(JSON.stringify({type: 'control_response', response: {subtype: 'success', request_id: row.request_id, response: {}}}) + '\n');
  } else if (row.type === 'user') {
    turns++;
    const bytes = Buffer.from(JSON.stringify({type: 'assistant', message: {content: [{type: 'text', text: '😀 Ω'}]}}) + '\n');
    const emoji = bytes.indexOf(Buffer.from('😀'));
    process.stdout.write(bytes.subarray(0, emoji + 1));
    await new Promise(resolve => setTimeout(resolve, 5));
    process.stdout.write(bytes.subarray(emoji + 1));
    process.stdout.write(JSON.stringify({type: 'result', subtype: 'success', result: `turn-${turns}`}) + '\n');
  }
}
process.stderr.write(`observed-eof-after-${turns}-results\n`);
