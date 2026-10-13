// Extracted source closure for 2.1.295; source module src_186460102.js sha256 1599a86cf60be2e07cf6da09f5d956ffc16e6ecb90ed05703992d72caf988557
import { G, UMt } from "./src_186056234.mjs";
import { Le } from "./src_186092222.mjs";
var ip="command-name";
var Mbe="bash-input";
var ape="bash-stdout";
var nG="bash-stderr";
var yxr="bash-exit-code";
var fl="local-command-stdout";
var rf="local-command-stderr";
var rG="local-command-caveat";
var W9e=[Mbe,ape,nG,yxr,fl,rf,rG];
var X3="tick";
var Du="task-notification";
var ej="teammate-message";
var yGn=64;
var FHt=100;
function $Ht(){if(process.env.CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC)return"essential-traffic";if(process.env.DISABLE_TELEMETRY)return"no-telemetry";if(Le(process.env.DO_NOT_TRACK))return"no-telemetry";return"default"}
function Rt(){return $Ht()==="essential-traffic"}
function J$(){return $Ht()!=="default"}
var z=100;
var Q=1000;
class x{recentErrors=[];queue=[];droppedBeforeSink=0;sink=null;hardFailMode=void 0;maxRememberedChars=void 0;remember(t){if(this.recentErrors.length>=z)this.recentErrors.shift();let e=this.maxRememberedChars;this.recentErrors.push(e===void 0||t.error.length<=e?t:{...t,error:`error text not remembered: it has ${t.error.length} characters, over this process's fixed limit of ${e} per error`})}dispatch(t){if(this.sink===null){if(this.queue.length>=Q){this.droppedBeforeSink++;return}this.queue.push(t);return}switch(t.type){case"error":this.sink.logError(t.error);break;case"mcpError":this.sink.logMCPError(t.serverName,t.error);break;case"mcpDebug":this.sink.logMCPDebug(t.serverName,t.message);break}}attachSink(t){if(this.sink!==null)return;this.sink=t;let e=this.queue;this.queue=[];for(let r of e)this.dispatch(r)}isHardFailMode(){return this.hardFailMode??=process.argv.includes("--hard-fail"),this.hardFailMode}reset(){this.recentErrors=[],this.queue=[],this.droppedBeforeSink=0,this.sink=null,this.hardFailMode=void 0,this.maxRememberedChars=void 0}}
var V9e=new x;
function c(t){let e=G(t);try{if(Le(process.env.CLAUDE_CODE_USE_BEDROCK)||Le(process.env.CLAUDE_CODE_USE_VERTEX)||Le(process.env.CLAUDE_CODE_USE_FOUNDRY)||Le(process.env.CLAUDE_CODE_USE_ANTHROPIC_AWS)||Le(process.env.CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD)||Le(process.env.CLAUDE_CODE_USE_MANTLE)||process.env.DISABLE_ERROR_REPORTING||Rt())return;let s={error:UMt(e),timestamp:new Date().toISOString()};V9e.remember(s),V9e.dispatch({type:"error",error:e})}catch{}}
export { c as c };
export { yGn as yGn };
export { ip as ip };
export { FHt as FHt };
export { W9e as W9e };
export { fl as fl };
export { rf as rf };
export { ape as ape };
export { nG as nG };
export { Du as Du };
export { X3 as X3 };
export { ej as ej };
export { J$ as J$ };
