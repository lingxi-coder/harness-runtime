// Extracted source closure for 2.1.295; source module src_186794983.js sha256 797b2dd2a4486d9adc71c2b33a78681b18306effcce43cbf9f64cfb7fe8e07a6
import { a } from "./src_186481843.mjs";
import { q, B, ept, S1e } from "./src_186096608.mjs";
var sxr={cli:!0,mcp:!0,"sdk-cli":!0,"sdk-ts":!0,"sdk-py":!0,bench:!0,"claude-vscode":!0,"claude-code-github-action":!0,"local-agent":!0,local_agent:!0,"claude-desktop":!0,remote:!0,remote_baku:!0,remote_cowork:!0,remote_trigger:!0,remote_cowork_trigger:!0,remote_desktop:!0,remote_mobile:!0,remote_projects:!0,claude_in_slack:!0,"claude-in-slack":!0,"claude-in-teams":!0,"claude-desktop-3p":!0,"claude-security":!0,"ssh-remote":!0,"claude-coworker":!0,"claude-coworker-terminal":!0};
var ITo=new Set(Object.keys(sxr));
function p(e){return ITo.has(e)}
function rp(){let e=a.CLAUDE_CODE_ENTRYPOINT;return e&&p(e)?e:void 0}
var r=new Set(["claude-desktop","claude-desktop-3p","local-agent"]);
function tu(){let e=a.CLAUDE_CODE_ENTRYPOINT;return e!==void 0&&r.has(e)}
var _=new Set(["claude-desktop-3p","local-agent"]);
function BUe(){let e=a.CLAUDE_CODE_ENTRYPOINT;return e!==void 0&&_.has(e)}
var axr=new Set(["claude-desktop","claude-desktop-3p"]);
function mD(){{let e=a.CLAUDE_CODE_ENTRYPOINT;return a.CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST&&a.CLAUDE_CODE_USE_GATEWAY&&(e!==void 0&&axr.has(e)||d())}return!1}
function d(){return a.CLAUDE_CODE_HOST_GATEWAY_LINEAGE&&!!a.CLAUDE_CODE_HOST_CREDS_FILE}
var s=new Set(["remote_cowork","remote_cowork_trigger"]);
function Kxe(){let e=UE();return e!==void 0&&s.has(e)}
function RHt(){let e=ept()?S1e():void 0;return e!==void 0&&s.has(e)}
function bZ(){return Kxe()||RHt()}
class E{entrypoint=void 0;interactive=!1;spawnedByAttendedSession=void 0;childSession=!1;hasRemoteSessionOrContainerId=!1;claudecode=!1;coworkFrameArtifacts=!1;hostScheduledRun=!1;desktopSkillSwitches=!1;hostUnattendedGrant=void 0;chromeTabGroupKey=void 0;hostAssignedGroup=void 0;hostSkillCatalog=void 0;remoteControlOwnerDeclared=!1;setEntrypoint(e){this.entrypoint=e}setInteractive(e){this.interactive=e}setSpawnedByAttendedSession(e){this.spawnedByAttendedSession=e}setChildSession(e){this.childSession=e}setHasRemoteSessionOrContainerId(e){this.hasRemoteSessionOrContainerId=e}setClaudecode(e){this.claudecode=e}setCoworkFrameArtifacts(e){this.coworkFrameArtifacts=e}setHostScheduledRun(e){this.hostScheduledRun=e}setDesktopSkillSwitches(e){this.desktopSkillSwitches=e}setHostUnattendedGrant(e){this.hostUnattendedGrant=e}setChromeTabGroupKey(e){this.chromeTabGroupKey=e}setHostSkillCatalog(e){this.hostSkillCatalog=e}setRemoteControlOwnerDeclared(e){this.remoteControlOwnerDeclared=e}setHostAssignedGroup(e){this.hostAssignedGroup=e}}
var O=new q(()=>new E);
function n(){return O.of(B().host)}
function UE(){return n().entrypoint}
export { tu as tu };
export { rp as rp };
export { bZ as bZ };
export { mD as mD };
export { BUe as BUe };
