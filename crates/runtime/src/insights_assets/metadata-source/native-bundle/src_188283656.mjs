// Extracted source closure for 2.1.295; source module src_188283656.js sha256 43f029eae110f449f9b8405cd2015b7fcfd1365c85a8c054c0f842f0a701237b
var i=new Set(["--prefill","--prefill-b64","--deep-link-repo","--deep-link-last-fetch","--deep-link-cwd-b64","--handle-uri","--settings","--managed-settings","--setting-sources","--client-data-url","--watch-artifact","--watch-artifact-no-autoreact","--team-name","--agent-id","--agent-name","--agent-color","--parent-session-id","--agent-type","--model","--agent","--routine","--effort","--permission-mode","--inherit-permission-mode","--proactivity","--debug-file","--system-prompt","--system-prompt-file","--append-system-prompt","--append-system-prompt-file","--system-prompt-snapshot","--append-subagent-system-prompt","--append-subagent-system-prompt-file","--plan-mode-instructions","--permission-prompt-tool","--permission-prompts","--json-schema","--fallback-model","--advisor","--agents","--name","-n","--allowedTools","--allowed-tools","--disallowedTools","--disallowed-tools","--tools","--add-dir","--mcp-config","--betas","--file","--channels","--dangerously-load-development-channels","--plugin-dir","--plugin-dir-no-mcp","--plugin-url","--remote-control-session-name-prefix","--sdk-url","--exec","-m","--thinking","--thinking-display","--max-thinking-tokens","--max-turns","--max-budget-usd","--task-budget","--autocompact","--rewind-files","--resume-session-at","--resume-drops-turn","--workload","--output-format","--input-format","--teammate-mode","--messaging-socket-path","--session-id","--environment","--pool","--ref","--on-branch","--correlation-id","--forward-home-settings","--project-config-root","--attach-serve",...[]]);
var sIr=new Set(["--allowedTools","--allowed-tools","--disallowedTools","--disallowed-tools","--tools","--add-dir","--mcp-config","--betas","--file","--channels","--dangerously-load-development-channels"]);
var a=new Set(["-d","--debug","-r","--resume","--from-pr","-w","--worktree","--teleport","--cloud","--remote","--project","--remote-control","--rc","--prompt-suggestions"]);
function apt(e,n){let t=e[n];if(t!==void 0&&a.has(t)){let r=e[n+1];return r!==void 0&&!AMt(r)?n+1:n}if(t===void 0||!i.has(t))return n;let o=n+1;if(sIr.has(t))while(o+1<e.length&&!AMt(e[o+1]))o++;return o}
function AMt(e){return e.length>1&&e.startsWith("-")}
function AH(e,n=process.argv){return Rpe(e,n).at(-1)}
function Rpe(e,n=process.argv){return l(e,n).map((t)=>t.value)}
function l(e,n){let t=[];for(let o=0;o<n.length;o++){let r=n[o];if(r==="--")break;if(r?.startsWith(`${e}=`)){t.push({value:r.slice(e.length+1),indices:[o]});continue}if(r===e&&o+1<n.length){t.push({value:n[o+1],indices:[o,o+1]}),o++;continue}o=apt(n,o)}return t}
function nA(e,n=process.argv){return lpt((t)=>t===e,n)!==-1}
function C8e(e=process.argv.slice(2)){let n=nA("-p",e)||nA("--print",e),t=nA("--init-only",e),o=lpt((r)=>r.startsWith("--sdk-url"),e)!==-1;return n||t||o||!process.stdout.isTTY}
function lpt(e,n=process.argv){for(let t=0;t<n.length;t++){let o=n[t];if(o==="--")break;if(o!==void 0&&e(o))return t;t=apt(n,t)}return-1}
export { AH as AH };
export { lpt as lpt };
export { C8e as C8e };
export { nA as nA };
