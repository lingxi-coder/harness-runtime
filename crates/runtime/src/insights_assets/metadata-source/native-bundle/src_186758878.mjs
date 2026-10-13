// Extracted source closure for 2.1.295; source module src_186758878.js sha256 af5c1d3d0124a123a4049d9d660ac0b64ce723f87b105d43eae0eb74c2dab6cb
var n=5;
function Rh(){let r=new Map;return{run(e,i){let t=(r.get(e)??Promise.resolve()).then(()=>i()),s=t.then(()=>{},()=>{});return r.set(e,s),s.then(()=>{if(r.get(e)===s)r.delete(e)}),t},has(e){return r.has(e)},get size(){return r.size},async settle(){await Promise.all([...r.values()])},async drain(){for(let e=0;e<n;e++){let i=[...r.values()];if(i.length===0)return;await Promise.all(i)}},clearForTest(){r.clear()}}}
export { Rh as Rh };
