// Extracted source closure for 2.1.295; source module src_186562918.js sha256 a5d294b6adfef2226ce0c1d083f048bfa6d5f04e49947c8ed0925eb3dc568675
class i{resetters=[];built=[];lazy(e){let t,r=()=>{t=void 0};return this.resetters.push(r),()=>{if(t===void 0)t=e(),this.built.push(r);return t}}builtCount(){return this.built.length}releaseBuiltSince(e){for(let t of this.built.splice(e))t()}reset(){for(let e of this.resetters)e();this.built.length=0}}
var FTr=new i;
function p(e){return FTr.lazy(e)}
export { p as p };
