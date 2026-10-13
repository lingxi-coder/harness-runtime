// Extracted source closure for 2.1.295; source module src_188290059.js sha256 0a7b7ae7991af264d89bc51382fef18ae35848cf0d67250aeb711cbba47fbe67
import { a } from "./src_186481843.mjs";
import { ys, q } from "./src_186096608.mjs";
import { Me } from "./src_186050226.mjs";
var R=null;
function Ese(){if(!R)R=Me("perf_hooks").performance;return R}
var I=a.CLAUDE_CODE_PROFILE_STARTUP;
var et=0.005;
var Y=Math.random()<et;
var P=I||Y;
class j{memorySnapshots=[];startupContext={};bootstrapEntry="cli";onceMarked=new Set;reported=!1;lateReported=!1;firstEmitPhases=new Set}
var nt=new q(()=>new j);
function A(){return ys(nt)}
function so(e,{once:s=!1}={}){if(!P)return!1;let{onceMarked:n,memorySnapshots:o}=A();if(s){if(n.has(e))return!1;n.add(e)}if(Ese().mark(e),I)o.push(process.memoryUsage());return!0}
export { so as so };
