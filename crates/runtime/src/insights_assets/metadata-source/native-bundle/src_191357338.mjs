// Extracted source closure for 2.1.295; source module src_191357338.js sha256 95a75797c2294c5201b6a141fc6ae520859c520908dbb046a24362ca3f30b4b2
import { AsyncLocalStorage as r } from "async_hooks";
import { q, B } from "./src_186096608.mjs";
class jbr{#e=!1;#n=!1;#r=!1;#t=!1;#s=!1;#a=!1;#o;get backgroundTasksDisabled(){return this.#e}get backgroundAgentLaunchDisabled(){return this.#n}get remoteAgentIsolationDisabled(){return this.#r}get unsandboxedCommandsDisabled(){return this.#t}get backgroundDeadlineDisabled(){return this.#s}get backgroundCompletionNoticeDisabled(){return this.#a}get backgroundLaunchRule(){return this.#o}disableBackgroundTasks(){this.#e=!0}disableBackgroundAgentLaunch(){this.#n=!0}disableRemoteAgentIsolation(){this.#r=!0}installBackgroundLaunchRule(e){if(!this.#e)throw Error("A background launch rule needs background tasks disabled first");if(typeof e.parameterDescription!=="string")throw Error("A background launch rule needs parameterDescription to be a string");if(this.#o!==void 0)throw Error("A background launch rule is already installed");this.#o=e}disableBackgroundDeadline(){this.#s=!0}disableBackgroundCompletionNotice(){this.#a=!0}disableUnsandboxedCommands(){this.#t=!0}}
var t=new q(()=>new jbr);
function ww(){return o.getStore()??t.of(B().host)}
var o=new r;
export { ww as ww };
