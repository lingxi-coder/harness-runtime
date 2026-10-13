// Extracted source closure for 2.1.295; source module src_190869821.js sha256 3ca14822617b19f032662e7b142a22de4ed2ba84de949ec2ebf756cabfbc3a76
import { join as i } from "path";
import { Ee } from "./src_186220896.mjs";
import { q, B } from "./src_186096608.mjs";
import { $E } from "./src_188118649.mjs";
function Cu(){return i(Ee(),"projects")}
class c{projectDirCache=new Map;agentTranscriptSubdirs=new Map;cacheProjectDir(t,e){this.projectDirCache.set(t,e)}clearProjectDirCache(){this.projectDirCache.clear()}setAgentTranscriptSubdir(t,e){this.agentTranscriptSubdirs.set(t,e)}clearAgentTranscriptSubdir(t){this.agentTranscriptSubdirs.delete(t)}}
var g=new q(()=>new c);
function r_(t){let e=g.of(B().host),n=e.projectDirCache.get(t);if(n!==void 0)return n;let r=i(Cu(),$E(t));return e.cacheProjectDir(t,r),r}
export { Cu as Cu };
export { r_ as r_ };
