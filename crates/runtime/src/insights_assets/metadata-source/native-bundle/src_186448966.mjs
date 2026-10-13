// Extracted source closure for 2.1.295; source module src_186448966.js sha256 5d7f8976d80606bb08e5e8f35be91fbbbda7026a41349f2edaa29a078d2733fd
import { D } from "./src_186447803.mjs";
function s(){return"prod"}
var mY=["","-staging-oauth","-local-oauth","-custom-oauth"];
function Y3(){if(process.env.CLAUDE_CODE_CUSTOM_OAUTH_URL)return"-custom-oauth";switch(s()){case"local":return"-local-oauth";case"staging":return"-staging-oauth";case"prod":return""}}
var yT="user:inference";
var gY="user:profile";
var _ut="user:plugins";
var uGn="user:projects:read";
var pGn="user:projects:write";
function fGn(t){return Array.isArray(t)&&t.includes(yT)}
var sp="oauth-2025-04-20";
var r=[gY,yT,"user:sessions:claude_code","user:mcp_servers","user:file_upload"];
function Sut(){let t=cn(),_=[...r];if(t.PLUGINS_SCOPE_REGISTERED)_.push(_ut);return _}
var n=[uGn,pGn];
function gxr(t){if(!Array.isArray(t))return[];let _=n;return t.filter((o)=>_.includes(o))}
function VTo(t){return D([...Sut(),...gxr(t)])}
var a={BASE_API_URL:"https://api.anthropic.com",CONSOLE_AUTHORIZE_URL:"https://platform.claude.com/oauth/authorize",CLAUDE_AI_AUTHORIZE_URL:"https://claude.com/cai/oauth/authorize",CLAUDE_AI_ORIGIN:"https://claude.ai",TOKEN_URL:"https://platform.claude.com/v1/oauth/token",API_KEY_URL:"https://api.anthropic.com/api/oauth/claude_cli/create_api_key",ROLES_URL:"https://api.anthropic.com/api/oauth/claude_cli/roles",CONSOLE_SUCCESS_URL:"https://platform.claude.com/buy_credits?returnUrl=/oauth/code/success%3Fapp%3Dclaude-code",CLAUDEAI_SUCCESS_URL:"https://platform.claude.com/oauth/code/success?app=claude-code",MANUAL_REDIRECT_URL:"https://platform.claude.com/oauth/code/callback",CLIENT_ID:"9d1c250a-e61b-44d9-88ed-5944d1962f5e",DESIGN_CLIENT_ID:"59637612-477b-4836-a601-b0589eda7704",OAUTH_FILE_SUFFIX:"",PLUGINS_SCOPE_REGISTERED:!0,PROJECTS_SCOPES_REGISTERED:!0,MCP_PROXY_URL:"https://mcp-proxy.anthropic.com",MCP_PROXY_PATH:"/v1/mcp/{server_id}"};
var u=void 0;
function i(){let t=process.env.CLAUDE_LOCAL_OAUTH_API_BASE?.replace(/\/$/,"")??"http://localhost:8000",_=process.env.CLAUDE_LOCAL_OAUTH_APPS_BASE?.replace(/\/$/,"")??"http://localhost:4000",o=process.env.CLAUDE_LOCAL_OAUTH_CONSOLE_BASE?.replace(/\/$/,"")??"http://localhost:3000";return{BASE_API_URL:t,CONSOLE_AUTHORIZE_URL:`${o}/oauth/authorize`,CLAUDE_AI_AUTHORIZE_URL:`${_}/oauth/authorize`,CLAUDE_AI_ORIGIN:_,TOKEN_URL:`${t}/v1/oauth/token`,API_KEY_URL:`${t}/api/oauth/claude_cli/create_api_key`,ROLES_URL:`${t}/api/oauth/claude_cli/roles`,CONSOLE_SUCCESS_URL:`${o}/buy_credits?returnUrl=/oauth/code/success%3Fapp%3Dclaude-code`,CLAUDEAI_SUCCESS_URL:`${o}/oauth/code/success?app=claude-code`,MANUAL_REDIRECT_URL:`${o}/oauth/code/callback`,CLIENT_ID:"22422756-60c9-4084-8eb7-27705fd5cf9a",DESIGN_CLIENT_ID:"00000000-0000-4000-8000-000000000000",OAUTH_FILE_SUFFIX:"-local-oauth",PLUGINS_SCOPE_REGISTERED:!0,PROJECTS_SCOPES_REGISTERED:!0,MCP_PROXY_URL:"http://localhost:8205",MCP_PROXY_PATH:"/v1/toolbox/shttp/mcp/{server_id}"}}
var zUe=["https://beacon.claude-ai.staging.ant.dev","https://claude.fedstart.com","https://claude-staging.fedstart.com"];
function hxr(){switch(s()){case"local":return i();case"staging":return u??a;case"prod":return a}}
function cn(){let t=hxr(),_=process.env.CLAUDE_CODE_CUSTOM_OAUTH_URL;if(_){let e=_.replace(/\/$/,"");if(!zUe.includes(e))throw Error("CLAUDE_CODE_CUSTOM_OAUTH_URL is not an approved endpoint.");t={...t,BASE_API_URL:e,CONSOLE_AUTHORIZE_URL:`${e}/oauth/authorize`,CLAUDE_AI_AUTHORIZE_URL:`${e}/oauth/authorize`,CLAUDE_AI_ORIGIN:e,TOKEN_URL:`${e}/v1/oauth/token`,API_KEY_URL:`${e}/api/oauth/claude_cli/create_api_key`,ROLES_URL:`${e}/api/oauth/claude_cli/roles`,CONSOLE_SUCCESS_URL:`${e}/oauth/code/success?app=claude-code`,CLAUDEAI_SUCCESS_URL:`${e}/oauth/code/success?app=claude-code`,MANUAL_REDIRECT_URL:`${e}/oauth/code/callback`,OAUTH_FILE_SUFFIX:"-custom-oauth",PLUGINS_SCOPE_REGISTERED:!1,PROJECTS_SCOPES_REGISTERED:!1}}let o=process.env.CLAUDE_CODE_OAUTH_CLIENT_ID;if(o)t={...t,CLIENT_ID:o,PLUGINS_SCOPE_REGISTERED:!1,PROJECTS_SCOPES_REGISTERED:!1};return t}
export { Y3 as Y3 };
export { mY as mY };
export { fGn as fGn };
export { sp as sp };
export { zUe as zUe };
export { VTo as VTo };
export { Sut as Sut };
export { cn as cn };
export { _ut as _ut };
