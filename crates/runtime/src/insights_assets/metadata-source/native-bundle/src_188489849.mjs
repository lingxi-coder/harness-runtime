// Extracted source closure for 2.1.295; source module src_188489849.js sha256 3644f4c7a483223a1289dc34e9426c718cfd35fbf729b62d86a049345e6f991b
import { Le } from "./src_186092222.mjs";
import { _V } from "./src_188182404.mjs";
class O{captured=E();carrier=R();ownSessionIds=y();get atStartup(){return this.captured}get ownCcrSessionIds(){return this.ownSessionIds}get bridgeCarrier(){return this.carrier}get isBridgeCarrierChild(){return this.carrier==="child"}reset(){this.captured=E(),this.carrier=R(),this.ownSessionIds=y()}}
var Zh=new O;
function zwo(){return Zh.bridgeCarrier?["CLAUDE_CODE_SESSION_ACCESS_TOKEN","SESSION_INGRESS_URL","CLAUDE_CODE_BRIDGE_PROMPT_SHA256"]:[]}
function E(){return process.env.SESSION_INGRESS_URL??process.env.ANTHROPIC_BASE_URL}
function R(){let e=process.env.CLAUDE_CODE_BRIDGE_MCP_CARRIER;if(e==="1"||!e)return e?"child":void 0;if(process.env.CLAUDE_CODE_ENVIRONMENT_KIND==="byoc"&&Le(process.env.CLAUDE_CODE_REMOTE)){delete process.env.CLAUDE_CODE_BRIDGE_MCP_CARRIER;return}return"descendant"}
function y(){return[process.env.CLAUDE_CODE_REMOTE_SESSION_ID,process.env.CLAUDE_CODE_SESSION_ID].filter((e)=>e!==void 0&&_V(e))}
export { zwo as zwo };
