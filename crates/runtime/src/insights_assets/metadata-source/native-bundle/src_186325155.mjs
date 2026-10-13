// Extracted source closure for 2.1.295; source module src_186325155.js sha256 a66f6f6d6f121ab2a8a9b5acf8ad8fa5d95ce3c6b55175dd7bbf5463b3567f0b
var g={Task:"Agent",KillShell:"TaskStop",KillBash:"TaskStop",ListPeers:"ListAgents",Brief:"SendUserMessage",ListMcpResources:"ListMcpResourcesTool",ReadMcpResource:"ReadMcpResourceTool",ReadMcpResourceDir:"ReadMcpResourceDirTool"};
function $l(e){return Object.hasOwn(g,e)?g[e]:e}
function zk(e){return e.includes("*")}
function y(e){return e.replaceAll("\\","\\\\").replaceAll("(","\\(").replaceAll(")","\\)")}
var x=/^[\\[\]!#()|+^$*?\s]$/;
function Enn(e){return x.test(e)}
var N=/^(?:[A-Za-z]:\\|~\\|\\(?![()!#]))/;
function MCo(e){return N.test(e)}
function DCo(e){return LCo(e).replaceAll("\\\\","\\")}
function LCo(e){return e.replaceAll("\\(","(").replaceAll("\\)",")")}
function Wue(e){if(typeof e==="string"&&!e.includes("(")&&!e.includes(")"))return{kind:"bare"};let n=b(e,"("),t=_(e,")");if(n===-1&&t===-1)return{kind:"bare"};let r=e.substring(0,n);if(n===-1||t<=n||t!==e.length-1||/[()]/.test(r))return{kind:"malformed"};return{kind:"call",toolName:r,rawContent:e.substring(n+1,t)}}
function $n(e){let n=Wue(e);if(n.kind!=="call"||!n.toolName)return{toolName:$l(e)};if(n.rawContent===""||n.rawContent==="*")return{toolName:$l(n.toolName)};let t=DCo(n.rawContent);return{toolName:$l(n.toolName),ruleContent:t}}
function bn(e){if(!e.ruleContent)return e.toolName;let n=y(e.ruleContent);return`${e.toolName}(${n})`}
function b(e,n){for(let t=0;t<e.length;t++)if(e[t]===n){let r=0,o=t-1;while(o>=0&&e[o]==="\\")r++,o--;if(r%2===0)return t}return-1}
function _(e,n){for(let t=e.length-1;t>=0;t--)if(e[t]===n){let r=0,o=t-1;while(o>=0&&e[o]==="\\")r++,o--;if(r%2===0)return t}return-1}
function mo(e){let n=e.split("__"),[t,r,...o]=n;if(t!=="mcp"||!r)return null;let s=o.length>0?o.join("__"):void 0;return{serverName:r,toolName:s}}
export { Wue as Wue };
export { $l as $l };
export { MCo as MCo };
export { LCo as LCo };
export { DCo as DCo };
export { Enn as Enn };
export { $n as $n };
export { mo as mo };
export { zk as zk };
export { bn as bn };
