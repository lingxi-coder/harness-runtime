// Extracted source closure for 2.1.295; source module src_186066580.js sha256 408b75b21246ffdc274c1eadb533f5dfbbe23597d158dbc9a7689fca5a9a141c
function KE({attempt:t,baseMs:r,capMs:e=1/0,factor:o=2,jitter:n={kind:"none"},random:m=Math.random}){let i=Math.min(e,r*o**(t-1));switch(n.kind){case"none":return i;case"proportional":return i+m()*n.ratio*i;case"symmetric":return Math.max(0,i+i*n.ratio*(2*m()-1));case"full":{let a=Math.min(n.floorMs,i);return a+m()*(i-a)}}}
class bIr{now(){return Date.now()}monotonicNow(){return performance.now()}setTimeout(t,r,e){let o=setTimeout(t,r);if(e?.unref)o.unref();return()=>clearTimeout(o)}}
var YE=new bIr;
function Q(t,r,e){return new Promise((o,n)=>{if(r?.aborted){if(e?.throwOnAbort||e?.abortError)n(e.abortError?.()??Error("aborted"));else o();return}let m=(e?.clock??YE).setTimeout(()=>{r?.removeEventListener("abort",i),o()},t,{unref:e?.unref});function i(){if(m(),e?.throwOnAbort||e?.abortError)n(e.abortError?.()??Error("aborted"));else o()}r?.addEventListener("abort",i,{once:!0})})}
var u=60000;
function PH({baseMs:t,capMs:r=u,attempt:e,floorMs:o=0,random:n=Math.random}){return Math.floor(KE({attempt:Math.max(0,e)+1,baseMs:t,capMs:r,jitter:{kind:"full",floorMs:o},random:n}))}
function c(t,r){t(Error(r))}
function $t(t,r,e){let o,n=new Promise((m,i)=>{o=setTimeout(c,r,i,e)});return Promise.race([t,n]).finally(()=>{if(o!==void 0)clearTimeout(o)})}
var s=(t,r)=>{let e=setTimeout(t,r);return()=>clearTimeout(e)};
function Xe(t,r,e=s){let o=()=>{},n=new Promise((m)=>{o=e(()=>m(void 0),r)});return Promise.race([t,n]).finally(o)}
export { bIr as bIr };
export { $t as $t };
export { Q as Q };
export { Xe as Xe };
export { PH as PH };
