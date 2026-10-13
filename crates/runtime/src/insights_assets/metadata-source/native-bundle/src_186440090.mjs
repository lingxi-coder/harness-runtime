// Extracted source closure for 2.1.295; source module src_186440090.js sha256 eeed128d75c671b4e90fd3e0ca7693e7e4c87bf2a4e6f3b939b59a68b5e99283
import { ys, q } from "./src_186096608.mjs";
var u=1000;
function l(){return{eventQueue:[],sink:null,droppedEventCount:0}}
function s(t,e){if(t.eventQueue.length>=u)t.eventQueue.shift(),t.droppedEventCount++;t.eventQueue.push(e)}
class r{state=l()}
var c=new q(()=>new r);
function a(){return ys(c)}
function i(t,e){let n=a().state;if(n.sink===null){s(n,{eventName:t,metadata:e,async:!1});return}n.sink.logEvent(t,e)}
export { i as i };
