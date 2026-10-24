import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';
const source = readFileSync(new URL('../../crates/runtime/src/mobile/unified_local_apps/browser_sdk.js', import.meta.url), 'utf8');
function browser({child=false, existing=false}={}) {
  const requests=[], timers=[], inputs=[];
  const config={appId:'app-one',name:'Meal',revision:3,runId:'epoch-one',services:['api']};
  let result={ok:true,value:{runId:'task-one',state:'running',sequence:1,updatedAt:10}};
  let failure=false;
  const window={}; window.top=child?{}:window;
  if(existing) window.localApp={desktop:true};
  const location={origin:'http://127.0.0.1:24131',href:''};
  const document={body:{append(){}}, createElement(name){
    const listeners={}; const input={style:{},setAttribute(){},addEventListener(event,handler){listeners[event]=handler;},click(){},remove(){input.removed=true;},listeners};
    inputs.push(input); return input;
  }};
  vm.runInNewContext(source.replace('__LINGXI_APP_CONFIGURATION__',JSON.stringify(config)), {
    window, location, document, console, URL, AbortSignal,
    fetch:async(url,options)=>{requests.push({url,options});if(failure)throw new Error('offline');return {ok:true,json:async()=>result};},
    setTimeout:fn=>{timers.push(fn);return timers.length;},clearTimeout:()=>{}
  });
  return {api:window.localApp,requests,timers,inputs,location,result:value=>{result=value;},offline:value=>{failure=value;}};
}
test('browser transport fixes app/run route and exposes no host authorization methods',async()=>{
  const b=browser(); await b.api.agent.run({requestId:'request-one',prompt:'describe'});
  assert.equal(b.requests[0].url,'http://127.0.0.1:24131/__run/epoch-one/__host/agent');
  assert.equal(b.requests[0].options.redirect,'error');
  assert.equal(b.requests[0].options.credentials,'omit');
  assert.equal(b.requests[0].options.headers['X-LingXi-App-Request'],'1');
  assert.deepEqual(JSON.parse(b.requests[0].options.body),{method:'agent.run',args:{requestId:'request-one',prompt:'describe'}});
  assert.equal((await b.api.info()).revision,3);
  assert.deepEqual(Object.keys(b.api.host),['open']);
  assert.equal(b.api.agent.answer,undefined);
  b.api.host.open(); assert.equal(b.location.href,'lingxi://apps/app-one?review=agent');
});
test('unknown service, traversal and encoded separators are rejected before network',async()=>{
  const b=browser();
  assert.equal(await b.api.serviceSocketUrl('api','/events?q=1'),'ws://127.0.0.1:24131/__run/epoch-one/__service/api/events?q=1');
  for(const path of ['/../secret','/%2e%2e/secret','/%2fsecret','//remote','/x#fragment','/%00'])assert.throws(()=>b.api.serviceUrl('api',path));
  assert.throws(()=>b.api.serviceUrl('other')); assert.equal(b.requests.length,0);
});
test('offline subscription reports host state and reconnects without resubmitting a task',async()=>{
  const b=browser(), events=[]; b.offline(true);
  const stop=b.api.agent.subscribe('task-one',event=>events.push(event));
  await new Promise(setImmediate);
  assert.equal(events[0].type,'connection'); assert.equal(events[0].error.code,'host_unavailable');
  b.offline(false); await b.timers.shift()();
  assert.equal(events[1].run.state,'running');
  b.result({ok:true,value:{runId:'task-one',state:'succeeded',sequence:2,updatedAt:11}});
  await b.timers.shift()(); stop();
  assert.equal(events[2].run.state,'succeeded'); assert.equal(b.timers.length,0);
  assert(b.requests.every(request=>JSON.parse(request.options.body).method==='agent.get'));
});
test('browser camera cancellation returns null and removes only its own input',async()=>{
  const b=browser(); const pending=b.api.camera.capture(); assert.equal(b.inputs.length,1);
  b.inputs[0].listeners.cancel(); assert.equal(await pending,null); assert.equal(b.inputs[0].removed,true);
  assert.equal(b.requests.length,0);
});
test('native Desktop SDK is preserved and child frames receive no host SDK',()=>{
  assert.equal(browser({existing:true}).api.desktop,true);
  assert.equal(browser({child:true}).api,undefined);
});
