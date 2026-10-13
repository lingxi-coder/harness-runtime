// Extracted source closure for 2.1.295; source module src_186559853.js sha256 8adcd0c9ff387507329c255807b111c5ab229c261e86683df879e3ac3ac0e902
import { q, B } from "./src_186096608.mjs";
import { Ie } from "./src_186092222.mjs";
import { D } from "./src_186447803.mjs";
var n=new Set(["hipaa"]);
class o{latched=new Set;latchedTaints=[];taints=[];current=[];changed=Ie();verdict=void 0;replaceTaints(e){this.current=D(e).sort();for(let r of e)if(n.has(r))this.latched.add(r);if(this.latched.size!==this.latchedTaints.length)this.latchedTaints=Array.from(this.latched).sort();return this.refreshEffectiveTaints()}latchTaints(e){let r=!1;for(let i of e)if(n.has(i)&&!this.latched.has(i))this.latched.add(i),r=!0;if(!r)return!1;return this.latchedTaints=Array.from(this.latched).sort(),this.refreshEffectiveTaints()}refreshEffectiveTaints(){let e=D([...this.latchedTaints,...this.current]).sort();if(e.length===this.taints.length&&e.every((r,i)=>r===this.taints[i]))return!1;return this.taints=e,this.changed.emit(this.taints),!0}registerVerdict(e){this.verdict=e}resetForTesting(){this.latched.clear(),this.latchedTaints=[],this.taints=[],this.current=[]}}
var a=new q(()=>new o);
function t(){return a.of(B().host)}
function Ya(e){return t().taints.includes(e)}
export { Ya as Ya };
