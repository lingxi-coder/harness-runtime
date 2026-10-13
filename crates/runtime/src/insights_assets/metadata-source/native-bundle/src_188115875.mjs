// Extracted source closure for 2.1.295; source module src_188115875.js sha256 4e4d142651d3ba46860445425449fa71e5f5b35779d32e90fc39022768660d11
var Wf=1e4;
async function Bi(e,r,t){let s=Math.max(1,Math.floor(t?.maxPages??1e4)),u;for(let a=0;a<s;a++){if(t?.budget!==void 0){if(t.budget.pagesLeft<1)return{status:"capped"};t.budget.pagesLeft--}let n=await e(u);if(!n.ok)return{status:"error",error:n.error};if(await r(n.value.items),t?.until?.())return{status:"done"};if(u=n.value.cursor,!u)return{status:"done"}}return{status:"capped"}}
export { Bi as Bi };
export { Wf as Wf };
