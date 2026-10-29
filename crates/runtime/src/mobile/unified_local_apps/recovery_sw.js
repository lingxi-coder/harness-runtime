/* SDK-owned per-App-origin offline page. Never caches application or Agent data. */
(() => {
  const config = __LOCAL_APP_RECOVERY_CONFIGURATION__;
  const cacheName = `lingxi-local-app-recovery:${config.appId}`;
  const recoveryUrl = new URL('/__lingxi-offline-recovery', self.location.origin).href;
  const diagnosticPlaceholder = '<div id="recovery-diagnostic" data-reason="" data-ready-marker="unavailable" hidden></div>';
  const recoveryDocument = `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; object-src 'none'">
<title>LingXi app paused</title><style>
body{font:16px/1.5 system-ui,sans-serif;max-width:42rem;margin:12vh auto;padding:0 1.25rem;color:#17202a;background:#f6f8fa}main{background:white;border:1px solid #d9e0e7;border-radius:16px;padding:1.4rem}h1{font-size:1.4rem}p{color:#425466}button,a.return{display:inline-block;padding:.7rem 1rem;border:0;border-radius:9px;background:#155eef;color:white;font:inherit;text-decoration:none}li{margin:.55rem 0;overflow-wrap:anywhere}code{font-size:.92em}
</style></head><body><main><h1>LingXi is paused or unavailable</h1>
<p>This saved page contains no prompt, files, or Agent result. Return to LingXi, then reopen this app in the external browser. Pending runs expire after 24 hours; accepted run receipts expire after 7 days. Expired records are deleted and are never sent again automatically.</p>
<p><a class="return" id="return" href="#">Return to LingXi</a></p>
<h2>Recovery diagnostics</h2><dl>
<dt>Reason</dt><dd id="recovery-reason">Not available for this cached page.</dd>
<dt>App ID</dt><dd id="recovery-app-id"></dd>
<dt>Revision</dt><dd id="recovery-revision"></dd>
<dt>Ready marker present</dt><dd id="recovery-ready-marker">Unavailable</dd>
</dl>
<h2>Runs saved in this browser</h2><p id="status">Checking this app's saved run IDs…</p><ul id="runs"></ul>
<div id="recovery-diagnostic" data-reason="" data-ready-marker="unavailable" hidden></div>
<script>
const config=__LOCAL_APP_RECOVERY_CONFIGURATION__;
const returnUrl='lingxi://apps/'+encodeURIComponent(config.appId);
document.querySelector('#return').href=returnUrl;
document.querySelector('#recovery-app-id').textContent=String(config.appId);
document.querySelector('#recovery-revision').textContent=String(config.revision);
const diagnostic=document.querySelector('#recovery-diagnostic');
if(diagnostic.dataset.reason){
 document.querySelector('#recovery-reason').textContent=/^\\d{3}$/u.test(diagnostic.dataset.reason)
  ?'HTTP '+diagnostic.dataset.reason
  :diagnostic.dataset.reason==='network_error'?'Network error':'Not available for this cached page.';
 document.querySelector('#recovery-ready-marker').textContent=diagnostic.dataset.readyMarker==='true'?'true':'false';
}
const list=document.querySelector('#runs');
const status=document.querySelector('#status');
function showMessage(text){status.textContent=text;}
if(!('indexedDB' in globalThis)){showMessage('This browser cannot read saved run IDs.');}
else{
 const opening=indexedDB.open('lingxi-local-app-agui-'+config.appId);
 opening.onupgradeneeded=()=>{if(!opening.result.objectStoreNames.contains('pending'))opening.result.createObjectStore('pending',{keyPath:'key'});};
 opening.onerror=()=>showMessage('Saved run IDs are unavailable in this browser.');
 opening.onblocked=()=>showMessage('Saved run IDs are busy in another tab.');
 opening.onsuccess=()=>{
  const db=opening.result;
  if(!db.objectStoreNames.contains('pending')){db.close();showMessage('There are no saved run IDs.');return;}
  const tx=db.transaction('pending','readwrite');
  const cursor=tx.objectStore('pending').openCursor();let count=0;let expired=0;
  cursor.onsuccess=()=>{
   const item=cursor.result;
   if(!item)return;
   const record=item.value;
   const accepted=record.state==='accepted';
   const timestamp=accepted?record.acceptedAt:record.createdAt;
   const ttl=accepted?7*24*60*60*1000:24*60*60*1000;
   if(record.appId===config.appId&&(!Number.isFinite(timestamp)||timestamp>Date.now()+5*60*1000||Date.now()-timestamp>=ttl)){
    item.delete();expired++;
   }else if(record.appId===config.appId&&record.revision===config.revision&&!accepted
      &&record.state!=='rejected'&&record.input&&typeof record.runId==='string'&&record.runId.length>0
      &&!/[\\u0000-\\u001f\\u007f-\\u009f]/u.test(record.runId)){
    const row=document.createElement('li');const id=document.createElement('code');
    id.textContent=record.runId;row.append(id);
    if(config.hostPlatform==='ios'&&record.state==='waking'
       &&record.key===config.appId+':'+config.revision+':'+record.runId
       &&typeof TextEncoder==='function'&&new TextEncoder().encode(record.runId).byteLength<=128
       &&new URLSearchParams({runId:record.runId}).get('runId')===record.runId){
     const query=new URLSearchParams({review:'agent',runId:record.runId,revision:String(config.revision)});
     const review=document.createElement('a');review.className='return';review.href=returnUrl+'?'+query.toString();
     review.textContent=' Review this run in LingXi';row.append(' ',review);
    }else{
     row.append(' — Reopen the app and check this run’s status before choosing an explicit same-ID retry.');
    }
    list.append(row);count++;
   }
   item.continue();
  };
  tx.oncomplete=()=>{db.close();showMessage((count?String(count)+' run ID(s) are saved. Return to LingXi to reopen the app or review an eligible run.':'There are no saved run IDs.')+(expired?' '+String(expired)+' expired record(s) were deleted.':''));};
  tx.onerror=()=>{db.close();showMessage('Saved run IDs are unavailable in this browser.');};
 };
}
</script></main></body></html>`;

  const htmlResponse = document => new Response(document, {
    status: 200,
    headers: {
      'Content-Type': 'text/html; charset=utf-8',
      'Cache-Control': 'no-store',
      'X-Content-Type-Options': 'nosniff'
    }
  });
  const cachedResponse = () => htmlResponse(recoveryDocument);

  const recoveryResponse = (request, reason) => {
    const readyMarker = new URL(request.url).searchParams.has('__lingxi_ready');
    const diagnosticReason = Number.isInteger(reason) ? String(reason) : reason === 'network_error' ? reason : '';
    const document = recoveryDocument.replace(diagnosticPlaceholder,
      `<div id="recovery-diagnostic" data-reason="${diagnosticReason}" data-ready-marker="${readyMarker}" hidden></div>`);
    return htmlResponse(document);
  };

  self.addEventListener('install', event => {
    event.waitUntil((async () => {
      const cache = await caches.open(cacheName);
      await cache.put(recoveryUrl, cachedResponse());
      await self.skipWaiting();
    })());
  });

  self.addEventListener('activate', event => {
    event.waitUntil(self.clients.claim());
  });

  self.addEventListener('fetch', event => {
    const request = event.request;
    if (request.method !== 'GET' || request.mode !== 'navigate') return;
    if (new URL(request.url).href === recoveryUrl) {
      event.respondWith(caches.open(cacheName).then(cache => cache.match(recoveryUrl)).then(response => response || cachedResponse()));
      return;
    }
    event.respondWith((async () => {
      let reason;
      const controller = new AbortController();
      let timer;
      let onAbort;
      try {
        const networkSignal = AbortSignal.any([request.signal, controller.signal]);
        const aborted = new Promise((_, reject) => {
          onAbort = () => {
            reject();
            controller.abort();
          };
          request.signal.addEventListener('abort', onAbort, {once: true});
          if (request.signal.aborted) onAbort();
        });
        const deadline = new Promise((_, reject) => {
          timer = setTimeout(() => {
            reject();
            controller.abort();
          }, 5000);
        });
        const network = (async () => {
          if (networkSignal.aborted) throw new Error('Navigation aborted.');
          return fetch(request, {signal: networkSignal});
        })();
        const response = await Promise.race([aborted, deadline, network]);
        if (response.ok || response.status < 400) return response;
        reason = response.status;
      } catch (_) {
        reason = 'network_error';
      } finally {
        clearTimeout(timer);
        request.signal.removeEventListener('abort', onAbort);
      }
      return recoveryResponse(request, reason);
    })());
  });
})();
