import assert from 'node:assert/strict';
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
const here = dirname(fileURLToPath(import.meta.url));
const casePath = resolve(here, '../fixtures/statusless-stream-error/cases.json');
const cases = JSON.parse(readFileSync(casePath, 'utf8'));
var vPe = function () { return false; };
function ht() {}
function An(e,r,n,s,h){if(s==="m")throw TypeError("Private method is not writable");if(s==="a"&&!h)throw TypeError("Private accessor was defined without a setter");if(typeof r==="function"?e!==r||!h:!r.has(e))throw TypeError("Cannot write private member to an object whose class did not declare it");return s==="a"?h.call(e,n):h?h.value=n:r.set(e,n),n}
function he(e,r,n,s){if(n==="a"&&!s)throw TypeError("Private accessor was defined without a getter");if(typeof r==="function"?e!==r||!s:!r.has(e))throw TypeError("Cannot read private member from an object whose class did not declare it");return n==="m"?s:n==="a"?s.call(e):s?s.value:r.get(e)}
function Xl(e){let r=0;for(let h of e)r+=h.length;let n=new Uint8Array(r),s=0;for(let h of e)n.set(h,s),s+=h.length;return n}
var Yl;
function WRt(e){let r;return(Yl??(r=new globalThis.TextEncoder,Yl=r.encode.bind(r)))(e)}
var Jl;
function GRt(e){let r;return(Jl??(r=new globalThis.TextDecoder,Jl=r.decode.bind(r)))(e)}
function Yr(e){if(e[Symbol.asyncIterator])return e;let r=e.getReader();return{async next(){try{let n=await r.read();if(n?.done)r.releaseLock();return n}catch(n){throw r.releaseLock(),n}},async return(){let n=r.cancel();return r.releaseLock(),await n,{done:!0,value:void 0}},[Symbol.asyncIterator](){return this}}}
var Je,Xe,Jr=(()=>{class e{constructor(){Je.set(this,void 0),Xe.set(this,void 0),An(this,Je,new Uint8Array,"f"),An(this,Xe,null,"f")}decode(r){if(r==null)return[];let n=r instanceof ArrayBuffer?new Uint8Array(r):typeof r==="string"?WRt(r):r;An(this,Je,Xl([he(this,Je,"f"),n]),"f");let s=[],h;while((h=Zf(he(this,Je,"f"),he(this,Xe,"f")))!=null){if(h.carriage&&he(this,Xe,"f")==null){An(this,Xe,h.index,"f");continue}if(he(this,Xe,"f")!=null&&(h.index!==he(this,Xe,"f")+1||h.carriage)){s.push(GRt(he(this,Je,"f").subarray(0,he(this,Xe,"f")-1))),An(this,Je,he(this,Je,"f").subarray(he(this,Xe,"f")),"f"),An(this,Xe,null,"f");continue}let b=he(this,Xe,"f")!==null?h.preceding-1:h.preceding,y=GRt(he(this,Je,"f").subarray(0,b));s.push(y),An(this,Je,he(this,Je,"f").subarray(h.index),"f"),An(this,Xe,null,"f")}return s}flush(){if(!he(this,Je,"f").length)return[];return this.decode(`
`)}}return Je=new WeakMap,Xe=new WeakMap,e.NEWLINE_CHARS=new Set([`
`,"\r"]),e.NEWLINE_REGEXP=/\r\n|[\n\r]/g,e})();
function Zf(e,r){for(let h=r??0;h<e.length;h++){if(e[h]===10)return{preceding:h,index:h+1,carriage:!1};if(e[h]===13)return{preceding:h,index:h+1,carriage:!0}}return null}
function iu(e){for(let s=0;s<e.length-1;s++){if(e[s]===10&&e[s+1]===10)return s+2;if(e[s]===13&&e[s+1]===13)return s+2;if(e[s]===13&&e[s+1]===10&&s+3<e.length&&e[s+2]===13&&e[s+3]===10)return s+4}return-1}
var Ni=(e)=>{try{return JSON.parse(e)}catch(r){return}}
var Kn=(()=>{class n extends Error{}return n})();class xt extends Kn{constructor(n,e,t,r,o){super(`${xt.makeMessage(n,e,t)}`);this.status=n,this.headers=r,this.requestID=r?.get("request-id"),this.workspaceID=r?.get("anthropic-workspace-id"),this.error=e,this.type=o??null}static makeMessage(n,e,t){let r=e?.message?typeof e.message==="string"?e.message:JSON.stringify(e.message):e?JSON.stringify(e):t;if(n&&r)return`${n} ${r}`;if(n)return`${n} status code (no body)`;if(r)return r;return"(no status code or body)"}static generate(n,e,t,r){if(!n||!r)return new yu({message:t,cause:HMt(e)});let o=e,i=o?.error?.type;if(n===400)return new MMt(n,o,t,r,i);if(n===401)return new A1e(n,o,t,r,i);if(n===403)return new DMt(n,o,t,r,i);if(n===404)return new T1e(n,o,t,r,i);if(n===409)return new LMt(n,o,t,r,i);if(n===422)return new NMt(n,o,t,r,i);if(n===429)return new FMt(n,o,t,r,i);if(n>=500)return new $Mt(n,o,t,r,i);return new xt(n,o,t,r,i)}}
async function*ou(e,r){if(!e.body){if(r.abort(),typeof globalThis.navigator<"u"&&globalThis.navigator.product==="ReactNative")throw new Kn("The default react-native fetch implementation does not support streaming. Please use expo/fetch: https://docs.expo.dev/versions/latest/sdk/expo/#expofetch-api");throw new Kn("Attempted to iterate over a response with no body")}let n=new su,s=new Jr,h=Yr(e.body);for await(let b of ep(h))for(let y of s.decode(b)){let w=n.decode(y);if(w)yield w}for(let b of s.flush()){let y=n.decode(b);if(y)yield y}}
async function*ep(e){let r=new Uint8Array;for await(let n of e){if(n==null)continue;let s=n instanceof ArrayBuffer?new Uint8Array(n):typeof n==="string"?WRt(n):n,h=new Uint8Array(r.length+s.length);h.set(r),h.set(s,r.length),r=h;let b;while((b=iu(r))!==-1)yield r.subarray(0,b),r=r.subarray(b)}if(r.length>0)yield r}
class su{constructor(){this.event=null,this.data=[],this.chunks=[]}decode(e){if(e.endsWith("\r"))e=e.substring(0,e.length-1);if(!e){if(!this.event&&!this.data.length)return null;let h={event:this.event,data:this.data.join(`
`),raw:this.chunks};return this.event=null,this.data=[],this.chunks=[],h}if(this.chunks.push(e),e.startsWith(":"))return null;let[r,n,s]=tp(e,":");if(s.startsWith(" "))s=s.substring(1);if(r==="event")this.event=s;else if(r==="data")this.data.push(s);return null}}
function tp(e,r){let n=e.indexOf(r);if(n!==-1)return[e.substring(0,n),r,e.substring(n+r.length)];return[e,"",""]}
var Xr;
class e {
constructor(r,n,s){this.iterator=r,Xr.set(this,void 0),this.controller=n,An(this,Xr,s,"f")}static rawEvents(r,n=new AbortController){return ou(r,n)}
static fromSSEResponse(r,n,s){let h=!1,b=s?Yh(s):console;async function*y(){if(h)throw new Kn("Cannot iterate over a consumed stream, use `.tee()` to split the stream.");h=!0;let w=!1;try{for await(let v of ou(r,n)){if(v.event==="completion")try{yield JSON.parse(v.data)}catch(O){throw b.error("Could not parse message into JSON:",v.data),b.error("From chunk:",v.raw),O}if(v.event==="message_start"||v.event==="message_delta"||v.event==="message_stop"||v.event==="content_block_start"||v.event==="content_block_delta"||v.event==="content_block_stop"||v.event==="message"||v.event==="user.message"||v.event==="user.interrupt"||v.event==="user.tool_confirmation"||v.event==="user.custom_tool_result"||v.event==="user.tool_result"||v.event==="agent.message"||v.event==="agent.thinking"||v.event==="agent.tool_use"||v.event==="agent.tool_result"||v.event==="agent.mcp_tool_use"||v.event==="agent.mcp_tool_result"||v.event==="agent.custom_tool_use"||v.event==="agent.thread_context_compacted"||v.event==="session.status_running"||v.event==="session.status_idle"||v.event==="session.status_rescheduled"||v.event==="session.status_terminated"||v.event==="session.error"||v.event==="session.deleted"||v.event==="session.updated"||v.event==="span.model_request_start"||v.event==="span.model_request_end"||v.event==="span.outcome_evaluation_start"||v.event==="span.outcome_evaluation_ongoing"||v.event==="span.outcome_evaluation_end"||v.event==="user.define_outcome"||v.event==="agent.thread_message_received"||v.event==="agent.thread_message_sent"||v.event==="agent.session_thread_message_received"||v.event==="agent.session_thread_message_sent"||v.event==="session.thread_created"||v.event==="session.thread_status_created"||v.event==="session.thread_status_running"||v.event==="session.thread_status_idle"||v.event==="session.thread_status_rescheduled"||v.event==="session.thread_status_terminated"||v.event==="event_start"||v.event==="event_delta"||v.event==="system.message")try{yield JSON.parse(v.data)}catch(O){throw b.error("Could not parse message into JSON:",v.data),b.error("From chunk:",v.raw),O}if(v.event==="ping")continue;if(v.event==="error"){let O=Ni(v.data)??v.data,F=O?.error?.type;throw new xt(void 0,O,void 0,r.headers,F)}}w=!0}catch(v){if(vPe(v))return;throw v}finally{if(!w)n.abort();ht(n)}}return new e(y,n,s)}
[(Xr=new WeakMap,Symbol.asyncIterator)](){return this.iterator()}
}

function utf16Hex(value) {
  let hex = '';
  for (let i = 0; i < value.length; i++) hex += value.charCodeAt(i).toString(16).padStart(4, '0');
  return hex;
}
async function runCase(fixture) {
  const response = new Response(Buffer.from(fixture.response_body_hex, 'hex'), {
    status: fixture.http_status,
    headers: { 'content-type': fixture.content_type },
  });
  const output = [];
  const textParts = [];
  const stream = e.fromSSEResponse(response, new AbortController(), null);
  let failure;
  try {
    for await (const item of stream) {
      output.push(item.type);
      if (item.type === 'content_block_delta' && item.delta && typeof item.delta.text === 'string') {
        textParts.push(item.delta.text);
      }
    }
  } catch (error) {
    failure = error;
    output.push('error');
  }
  assert.ok(failure, 'the terminal event:error must throw');
  const nestedMessage = failure.error && failure.error.error && failure.error.error.message;
  const rawSse = Buffer.from(fixture.response_body_hex, 'hex').toString('utf8');
  let currentEvent = null;
  let rawErrorData = '';
  for (const line of rawSse.split(/\r?\n/)) {
    if (line.startsWith('event: ')) currentEvent = line.slice(7);
    if (currentEvent === 'error' && line.startsWith('data: ')) {
      rawErrorData = line.slice(6);
      break;
    }
  }
  const result = {
    fixture_id: fixture.id,
    execution_label: 'extracted-295 execution; not public Native',
    emitted_event_sequence: output,
    expected_event_sequence: fixture.expected_event_sequence || ['error'],
    pre_error_text: textParts.join(''),
    expected_pre_error_text: fixture.pre_error_text || '',
    api_error_status_is_undefined: failure.status === undefined,
    api_error_type: failure.type,
    api_error_message: failure.message,
    api_error_message_utf16_hex: utf16Hex(failure.message),
    raw_error_data_source: rawErrorData,
    api_error_message_equals_raw_sse_data: failure.message === rawErrorData,
    nested_error_message: nestedMessage,
    nested_error_message_utf16_hex: utf16Hex(nestedMessage),
    expected_nested_error_message_utf16_hex: fixture.error_message_utf16_units_hex,
  };
  assert.deepEqual(result.emitted_event_sequence, result.expected_event_sequence);
  assert.equal(result.pre_error_text, result.expected_pre_error_text);
  assert.equal(result.api_error_status_is_undefined, true);
  assert.equal(result.api_error_type, fixture.error_type);
  assert.equal(result.api_error_message_equals_raw_sse_data, true);
  assert.equal(result.nested_error_message_utf16_hex, fixture.error_message_utf16_units_hex);
  assert.equal(result.api_error_message, JSON.stringify(failure.error));
  return result;
}
const outcome = [];
for (const fixture of cases.cases) outcome.push(await runCase(fixture));
const outputPath = resolve(here, 'probe-output.json');
writeFileSync(outputPath, JSON.stringify(outcome, null, 2) + '\n');
console.log(JSON.stringify(outcome, null, 2));
