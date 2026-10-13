// Extracted source closure for 2.1.295; source module src_186092222.js sha256 aacaa8fe4c03be1a305899c7c9cab668105e8fa89dac7450a3361c43431bf2cb
function Le(e){if(!e)return!1;if(typeof e==="boolean")return e;let n=String(e).toLowerCase().trim();return["1","true","yes","on"].includes(n)}
function _s(e){if(e===void 0)return!1;if(typeof e==="boolean")return!e;let n=String(e).toLowerCase().trim();return["0","false","no","off"].includes(n)}
var s=globalThis.process?.getBuiltinModule?.("async_hooks");
var Asn=s?(e)=>s.AsyncResource.bind(e):(e)=>e;
function Ie(){let e=new Set;return{subscribe(n){let r=Asn(n);return e.add(r),()=>{e.delete(r)}},emit(...n){let r;for(let o of e)try{o(...n)}catch(i){(r??=[]).push(i)}if(r)throw r.length===1?r[0]:AggregateError(r,"Signal listener(s) threw")},clear(){e.clear()}}}
export { Ie as Ie };
export { Le as Le };
export { Asn as Asn };
export { _s as _s };
