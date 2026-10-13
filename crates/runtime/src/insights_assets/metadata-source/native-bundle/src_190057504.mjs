// Extracted source closure for 2.1.295; source module src_190057504.js sha256 899f371142cb345e74ba3c177eb3b8ed896963303a40004cdea40d08e6997297
import { Asn } from "./src_186092222.mjs";
function us(t,e){let r=t,S=new Set,a=e&&Asn(e),s=()=>r;return{getSnapshot:s,getState:s,setState:(n)=>{let o=r,i=n(o);if(Object.is(i,o))return;r=i,a?.({newState:i,oldState:o});for(let u of S)u()},subscribe:(n)=>{let o=Asn(n);return S.add(o),()=>S.delete(o)}}}
export { us as us };
