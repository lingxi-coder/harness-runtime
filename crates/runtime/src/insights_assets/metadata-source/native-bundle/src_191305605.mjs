// Extracted source closure for 2.1.295; source module src_191305605.js sha256 6132fd0d55e1665730b8725888d03c889ad0544dda309f29c4f1442830e23dc8
import { URL as U } from "url";
import { BlockList as de, isIP as z, isIP as T, isIP as q, BlockList as fe, connect as V } from "net";
import { homedir as L } from "os";
import * as O from "path";
import * as D from "fs";
import { connect as pe } from "tls";
function rps(){return}
function pw(){switch("darwin"){case"darwin":return"macos";case"linux":return"linux";case"win32":return"windows";default:return"unknown"}}
function Pt(e,t){if(!process.env.SRT_DEBUG)return;let r=t?.level||"info",n="[SandboxDebug]";switch(r){case"error":console.error(`${n} ${e}`);break;case"warn":console.warn(`${n} ${e}`);break;default:console.error(`${n} ${e}`)}}
var pHn=[".gitconfig",".gitmodules",".bashrc",".bash_profile",".zshrc",".zprofile",".profile",".ripgreprc",".mcp.json"];
var ee=[".git",".vscode",".idea"];
function fHn(){return[...ee.filter((e)=>e!==".git"),".claude/commands",".claude/agents"]}
function zao(e){return e.toLowerCase()}
function Hx(e,t){return e===t||e.startsWith(t==="/"?"/":t+"/")}
function CAt(e,t){return e!==t&&Hx(e,t)}
function*Vao(e){for(let t=e.lastIndexOf("/");t>0;t=e.lastIndexOf("/",t-1))yield e.slice(0,t);if(e!=="/")yield"/"}
function ops(e){try{let t=D.realpathSync(e);if(t!==e)return[e,t]}catch{}return[e]}
function mHn(e){let t=e?.code;return t==="ENOENT"||t==="ENOTDIR"||t==="ELOOP"||t==="ENAMETOOLONG"}
function nd(e){return e.includes("*")||e.includes("?")||e.includes("[")||e.includes("]")}
function AAt(e){return e.includes("*")||e.includes("?")}
function gHn(e){return pw()==="windows"?AAt(e):nd(e)}
function lre(e){if(/^\\\\\?\\unc\\/i.test(e))return"\\\\"+e.slice(8);if(e.startsWith("\\\\?\\"))return e.slice(4);return e}
function kgr(e){let t=O.win32.toNamespacedPath(e);return/^[\\/]{2}[?.][\\/]unc[\\/]/i.test(t)}
function km(e){return e.replace(/\/\*\*$/,"")||"/"}
function z9t(e,t){let r=O.normalize(e),n=O.normalize(t);if(n===r)return!1;if(r.startsWith("/tmp/")&&n==="/private"+r)return!1;if(r.startsWith("/var/")&&n==="/private"+r)return!1;if(r.startsWith("/private/tmp/")&&n===r)return!1;if(r.startsWith("/private/var/")&&n===r)return!1;if(n==="/")return!0;if(n.split("/").filter(Boolean).length<=1)return!0;if(r.startsWith(n+"/"))return!0;let i=r;if(r.startsWith("/tmp/"))i="/private"+r;else if(r.startsWith("/var/"))i="/private"+r;if(i!==r&&i.startsWith(n+"/"))return!0;let s=n.startsWith(r+"/"),c=i!==r&&n.startsWith(i+"/");if(n!==r&&!(i!==r&&n===i)&&!s&&!c)return!0;return!1}
function Cgr(e){if(e==="~")return L();if(e.startsWith("~/"))return L()+e.slice(1);return e}
function qao(e){let t=L(),r=/^[A-Za-z]:/.test(t)?t.slice(0,2):"",n=r?t.slice(2):t;return e.replace(/%USERPROFILE%/gi,t).replace(/%HOMEDRIVE%/gi,r).replace(/%HOMEPATH%/gi,n)}
function te(e){return e.replace(/\/{2,}/g,"/").replace(/\/\.(?=\/)/g,"")}
function A(e){if(pw()!=="windows"&&/(?:^|\/)\.\.(?:\/|$)/.test(e))Pt(`[Sandbox] "${e}" could not be resolved and still contains a ".." component, so a rule spelled this way may not match.`,{level:"warn"});return e}
function wv(e,t){let r=(o)=>!t?.literal&&gHn(o);if(pw()==="windows"){if(e=lre(qao(e)),/^[a-z]:/.test(e))e=e[0].toUpperCase()+e.slice(1);if(kgr(e)&&!r(e))return O.win32.normalize(e)}if(pw()!=="windows"&&e.endsWith("/")&&!r(e))e=e.replace(/\/+$/,"")||"/";let n=Cgr(e);if(n!==e);else if(e.startsWith("./")||e.startsWith("../"))n=O.resolve(process.cwd(),e);else if(!O.isAbsolute(e))n=O.resolve(process.cwd(),e);if(pw()!=="windows")n=te(n);if(r(n)){let o=pw()==="windows"?/[*?]/:/[*?[\]]/,i=n.split(o)[0];if(i&&i!=="/"){let s=i.endsWith("/")?i.slice(0,-1):O.dirname(i);try{let c=D.realpathSync(s);if(!z9t(s,c)){let f=n.slice(s.length);return A(c+f)}}catch{}}return A(n)}if(pw()!=="windows"&&n!=="")n=n.replace(/\/\.$/,"").replace(/\/+$/,"")||"/";try{let o=D.realpathSync(n);if(z9t(n,o));else n=o}catch{}return A(n)}
var ne=["/dev/stdout","/dev/stderr","/dev/null","/dev/tty","/dev/dtracehelper","/dev/autofs_nowait","/tmp/claude","/private/tmp/claude"];
var X=[".npm/_logs",".claude/debug"];
function DF(e){let t=L(),r=!e||e.denyRead.length===0?X:re(t,e.denyRead,e.allowRead);return[...ne,...r.map((n)=>O.join(t,n))]}
function re(e,t,r=[]){let n=[...new Set([e,wv(e,{literal:!0})])],o=t.map((s)=>se(s)),i=r.map((s)=>km(s)).filter((s)=>!gHn(s)).map((s)=>wv(s,{literal:!0}));return X.filter((s)=>{let c=n.map((f)=>O.join(f,s));return!o.some((f)=>c.some(f)&&!i.some((a)=>f(a)&&c.some((g)=>Hx(g,a))))})}
function se(e){let t=km(e);if(gHn(t))try{let n=new RegExp(K9t(wv(t)));return(o)=>n.test(o)}catch{}let r=wv(t,{literal:!0});return(n)=>Hx(n,r)}
var oe=["NODE_EXTRA_CA_CERTS","SSL_CERT_FILE","CURL_CA_BUNDLE","REQUESTS_CA_BUNDLE","PIP_CERT","GIT_SSL_CAINFO","AWS_CA_BUNDLE","CARGO_HTTP_CAINFO","DENO_CERT","CLOUDSDK_CORE_CUSTOM_CA_CERTS_FILE","NIX_SSL_CERT_FILE"];
function V9t(e,t,r,n,o,i){let s=ce(i),c=s===aLe?s:encodeURIComponent(s),f=n?`${c}:${n}@`:"",a=["SANDBOX_RUNTIME=1"];if(!o){let C=process.env.CLAUDE_CODE_TMPDIR||process.env.CLAUDE_TMPDIR||"/tmp/claude";a.push(`TMPDIR=${C}`)}if(r)for(let C of oe)a.push(`${C}=${r}`);if(!e&&!t)return a;let g=["localhost","127.0.0.1","::1","169.254.0.0/16","10.0.0.0/8","172.16.0.0/12","192.168.0.0/16"].join(",");if(a.push(`NO_PROXY=${g}`),a.push(`no_proxy=${g}`),e){if(a.push(`HTTP_PROXY=http://${f}localhost:${e}`),a.push(`HTTPS_PROXY=http://${f}localhost:${e}`),a.push(`http_proxy=http://${f}localhost:${e}`),a.push(`https_proxy=http://${f}localhost:${e}`),n)a.push("GIT_CONFIG_PARAMETERS='http.proxyAuthMethod=basic'")}let x=e?`http://${f}localhost:${e}`:`socks5h://${f}localhost:${t}`;if(a.push(`ALL_PROXY=${x}`),a.push(`all_proxy=${x}`),a.push(`GRPC_PROXY=${x}`),a.push(`grpc_proxy=${x}`),t){let y=pw();if(y==="macos")a.push(`GIT_SSH_COMMAND=ssh -o ControlMaster=no -o ControlPath=none -o ProxyCommand='nc -X 5 -x localhost:${t} %h %p'`);else if(y==="linux"&&e){let S=n?`,proxyauth=${s}:${n}`:"";a.push(`GIT_SSH_COMMAND=ssh -o ControlMaster=no -o ControlPath=none -o ProxyCommand='socat - PROXY:localhost:%h:%p,proxyport=${e}${S}'`)}if(a.push(`FTP_PROXY=socks5h://${f}localhost:${t}`),a.push(`ftp_proxy=socks5h://${f}localhost:${t}`),a.push(`RSYNC_PROXY=localhost:${t}`),a.push(`DOCKER_HTTP_PROXY=http://${f}localhost:${e||t}`),a.push(`DOCKER_HTTPS_PROXY=http://${f}localhost:${e||t}`),e){if(a.push("CLOUDSDK_PROXY_TYPE=http"),a.push("CLOUDSDK_PROXY_ADDRESS=localhost"),a.push(`CLOUDSDK_PROXY_PORT=${e}`),n)a.push(`CLOUDSDK_PROXY_USERNAME=${s}`),a.push(`CLOUDSDK_PROXY_PASSWORD=${n}`)}}return a}
var ie=8;
function Agr(e){if(e.baseEnv?.GIT_CONFIG_COUNT==="0")return{};let t=Number.parseInt(e.baseEnv?.GIT_CONFIG_COUNT??"",10),r=Number.isFinite(t)&&t>=0?t:0,n=r,o={},i=(c,f)=>{o[`GIT_CONFIG_KEY_${n}`]=c,o[`GIT_CONFIG_VALUE_${n}`]=f,n++},s=[...new Set(e.safeDirs.filter((c)=>!!c).map((c)=>{let a=c.replace(/\\/g,"/").replace(/\/+$/,"");if(a===""||/^[A-Za-z]:$/.test(a))return`${a}/`;return a}))];if(s.length>ie)i("safe.directory","*");else for(let c of s)i("safe.directory",c),i("safe.directory",c.endsWith("/")?`${c}*`:`${c}/*`);if(e.schannelCa)i("http.schannelUseSSLCAInfo","true"),i("http.schannelCheckRevoke","false");if(n===r)return{};return o.GIT_CONFIG_COUNT=String(n),o}
function Tgr(e){let t={GIT_CONFIG_COUNT:process.env.GIT_CONFIG_COUNT};for(let r of e.unsetEnvVars??[])delete t[r];return Object.assign(t,e.setEnvVars??{}),Agr({safeDirs:e.safeDirs,baseEnv:t})}
var Kao=100;
function zot(e,t){return!t?e:t}
function Ece(e){let t=e.slice(0,Kao);return Buffer.from(t).toString("base64")}
function iLe(e){return Buffer.from(e,"base64").toString("utf8")}
var aLe="srt";
var Y=255;
var Yao=Y-Buffer.byteLength(`${aLe}.`);
function ce(e){if(!e)return aLe;let t=`${aLe}.${e}`;return Buffer.byteLength(t)<=Y?t:aLe}
function hHn(e){if(!e||!e.startsWith(`${aLe}.`))return;let t=e.slice(aLe.length+1);if(!t||Buffer.byteLength(t)>Yao)return;return t}
function q9t(e){return"^"+e.replace(/[.^$+{}()|\\]/g,"\\$&").replace(/\[([^\]]*?)$/g,"\\[$1").replace(/\*\*\//g,"__GLOBSTAR_SLASH__").replace(/\*\*/g,"__GLOBSTAR__").replace(/\*/g,"[^/]*").replace(/\?/g,"[^/]").replace(/__GLOBSTAR_SLASH__/g,"(.*/)?").replace(/__GLOBSTAR__/g,".*")+"$"}
function K9t(e){return q9t(e).slice(0,-1)+"(/.*)?$"}
function Y9t(e,t={}){return Jao(e,t).matches}
function H(e,t){if(e.includes("__GLOBSTAR"))return;let r=(s)=>q9t(s).slice(1,-1),n=[],o=0,i=/(\[[^\]]*\])|(\*+)(\/?)|(\/)|([^[*/]+|\[)/g;for(let s of e.matchAll(i)){let[c,f,a,g,x]=s;if(f!==void 0){if(/[*?]/.test(f))return;let C=r(f);n.push(new RegExp(`^${C}$`,t).test("/")?{source:C,canBeSeparator:!0}:{source:C})}else if(a!==void 0){let C=g==="/"&&a.length>=2,y=a.length-(C?2:0);if(y>=2)n.push("any");else if(y===1)n.push({source:"[^/]*"});if(C)n.push("anyDirs");else if(g==="/")n.push("/")}else if(x!==void 0)n.push("/");else{if(c==="["&&o++>0)return;n.push({source:r(c)})}}return n.filter((s,c)=>s!=="anyDirs"||n[c+1]!=="anyDirs"&&n[c+1]!=="any")}
function ae(e,t){let r=new RegExp(q9t(e),t),n=km(e),o=n!==e?new RegExp(q9t(n),t):void 0,i={splits:!1,start:[0],next:()=>[0],matches:(u,l,d)=>r.test(d),matchesDirectoryForm:(u,l,d)=>o?.test(d)===!0},s=[],c="[^/]*",f=(u)=>{let l=new Map,d=(w)=>s.push({then:w})-1,h=(w,p)=>{let m=`${w}${p?"*":""}`,b=l.get(m);if(b!==void 0)return b;if(!p&&u[w]==="anyDirs"){let E=d(void 0);return l.set(m,E),s[E].then=h(w+1,!1),E}if(!p&&u[w]==="any"&&w===u.length-1){let E=d(void 0);return l.set(m,E),E}let I={edges:[]},N=s.push(I)-1;l.set(m,N);let v=(E,_)=>{I.edges.push({regex:new RegExp(`^${E}$`,t),to:_})},R=p?"[^/]*":"";for(let E=w;;E++){let _=u[E];if(_===void 0){v(R,void 0);break}if(_==="/"){v(R,h(E+1,!1));break}if(_==="any")v(R+"[^/]*",d(h(E+1,!0))),R+="[^/]*";else if(_==="anyDirs")v(R+"[^/]*",d(h(E+1,!1)));else if(_.canBeSeparator)v(R,h(E+1,!1)),R+=`(?!/)${_.source}`;else R+=_.source}return N};return h(0,!1)},a,g;try{let u=H(e,t),l=o?H(n,t):[];if(u===void 0||l===void 0)return i;if(a=[f(u)],g=s.length,o)a.push(f(l))}catch{return i}let x=(u,l)=>{for(let d=l;d!==void 0&&!u.has(d);){u.add(d);let h=s[d];d="then"in h?h.then:void 0}},C=(u)=>[...u].sort((l,d)=>l-d),y=(u,l,d)=>u.some((h)=>{if(h>=g!==d)return!1;let w=s[h];return"then"in w?w.then===void 0:w.edges.some((p)=>p.to===void 0&&p.regex.test(l))}),S=new Set;for(let u of a)x(S,u);return{splits:!0,start:C(S),next:(u,l)=>{let d=new Set;for(let h of u){let w=s[h];if("then"in w)x(d,h);else for(let p of w.edges)if(p.to!==void 0&&p.regex.test(l))x(d,p.to)}return C(d)},matches:(u,l)=>y(u,l,!1),matchesDirectoryForm:(u,l)=>y(u,l,!0)}}
function Xao(e){let t=e.split(/[*?[\]]/)[0];if(!t)return"";return t.endsWith("/")?t.slice(0,-1):O.dirname(t)}
function k(e){return e}
function Jao(e,t={}){let r={baseLocation:"",matches:[],directoryMatches:[],symlinks:new Set,uninspectableLinks:new Set,unlisted:[],realOf:new Map},n=k(wv(e)),o=Xao(n);if(o===""||o==="/")return Pt(`[Sandbox] Glob pattern has no literal directory to start from, skipping: ${e}`,{level:"warn"}),r;let i=t.caseInsensitive?"is":"s",s=ae(n,i);if(t.followSymlinkedDirectories&&!s.splits)Pt(`[Sandbox] Glob pattern ${e} cannot be followed one path component at a time, so it is matched against real paths only and not through symlinked directories`,{level:"warn"});let c=new Map,f=new Map,a=[],g=(S,u,l)=>{try{return l(S)}catch(d){if(u===S)throw d;return l(u)}},x=new Map,C=(S,u)=>{let l=x.get(u);if(l!==void 0)return l;let d;try{d=g(u,S,(h)=>({isDirectory:D.statSync(h).isDirectory(),real:D.realpathSync(h)}))}catch(h){d=mHn(h)?"absent":"uninspectable"}return x.set(u,d),d},y=o;try{y=D.realpathSync(o)}catch{}r.baseLocation=y,a.push({dir:o,real:y,short:o.length<y.length?o:y,positions:o.split("/").reduce(s.next,s.start)});for(let S=a.pop();S!==void 0;S=a.pop()){let{dir:u,real:l}=S,d=c.get(l);if(d===void 0)c.set(l,d=new Set);let h=S.positions.filter((p)=>!d.has(p));if(h.length===0)continue;for(let p of h)d.add(p);let w=f.get(l);try{w??(w=g(l,S.short,(p)=>D.readdirSync(p,{withFileTypes:!0}))),f.set(l,w)}catch(p){let m=p?.code;if(Pt(`[Sandbox] Error listing ${u} for glob pattern ${e}: ${p}`,{level:m==="ENOENT"?"info":"warn"}),m!=="ENOENT"){if(r.unlisted.push(u),l!==u)r.realOf.set(u,l)}continue}for(let p of w){let m=O.join(u,p.name),b=O.join(l,p.name),I=k(m),N=s.matches(h,p.name,I);if(N)r.matches.push(m);if(p.isDirectory()){let M=s.next(h,p.name),B=t.withDirectoryForm===!0&&s.matchesDirectoryForm(h,p.name,I);if(B)r.directoryMatches.push(m);if((N||B)&&b!==m)r.realOf.set(m,b);if(M.length>0)a.push({dir:m,real:b,short:O.join(S.short,p.name),positions:M});continue}if(!p.isSymbolicLink()){if(N&&b!==m)r.realOf.set(m,b);continue}if(r.symlinks.add(m),!t.followSymlinkedDirectories)continue;let v=t.withDirectoryForm===!0&&s.matchesDirectoryForm(h,p.name,I),R=s.splits?s.next(h,p.name):[];if(!N&&!v&&R.length===0)continue;let E=O.join(S.short,p.name),_=C(E,b);if(_==="absent")continue;if(_==="uninspectable"){if(r.uninspectableLinks.add(m),v)r.directoryMatches.push(m);continue}if(N)r.realOf.set(m,_.real);if(!_.isDirectory)continue;if(v)r.directoryMatches.push(m),r.realOf.set(m,_.real);if(R.length===0)continue;if(Hx(l,_.real)||Hx(y,_.real)){Pt(`[Sandbox] Not following symlink ${m} -> ${_.real} for glob pattern ${e}: it leads back up the tree`);continue}a.push({dir:_.real,real:_.real,short:E.length<_.real.length?E:_.real,positions:R})}}return r}
var Qao=["127.0.0.0/8","::1"];
function j(e){let t=z(e);return t===4?"ipv4":t===6?"ipv6":void 0}
function Zao(e){let t=e.indexOf("/"),r=W(t===-1?e:e.slice(0,t)),n=j(r);if(!n)return;let o=n==="ipv6"?128:32,i=o;if(t!==-1){let c=e.slice(t+1);if(!/^\d{1,3}$/.test(c)||Number(c)>o)return;i=Number(c)}let s=n==="ipv6"?G(r):void 0;if(s!==void 0)return i<96?void 0:{address:s,prefix:i-96,family:"ipv4"};return{address:r,prefix:i,family:n}}
function TAt(e,t){let r=Zao(t);if(!r)return!1;try{e.addSubnet(r.address,r.prefix,r.family)}catch{return!1}return!0}
function ue(e){let t=new fe;for(let r of e)if(!TAt(t,r))throw Error(`Invalid IP address or CIDR range: ${JSON.stringify(r)}`);return t}
function Vot(e,t){let r=W(t),n=j(r);return n!==void 0&&e.check(r,n)}
function W(e){let t=e.indexOf("%");return t===-1?e:e.slice(0,t)}
function K(e){let t;try{t=new URL(`http://[${W(e)}]/`).hostname.slice(1,-1)}catch{return}let[r="",n]=t.split("::"),o=r?r.split(":"):[],i=n?n.split(":"):[],s=n===void 0?0:8-o.length-i.length;if(s<0||o.length+s+i.length!==8)return;return[...o,...Array(s).fill("0"),...i].map((c)=>parseInt(c,16))}
var P=(e,t)=>`${e>>8}.${e&255}.${t>>8}.${t&255}`;
function G(e){if(!/ffff/i.test(e))return;let t=K(e);return t&&t.slice(0,5).every((r)=>r===0)&&t[5]===65535?P(t[6],t[7]):void 0}
function sps(e){if(z(e)!==6)return;let t=K(e);if(!t)return;let r=(o,i)=>t.slice(o,i).every((s)=>s===0);if(t[0]===8194)return P(t[1],t[2]);let n=P(t[6],t[7]);if(t[0]===100&&t[1]===65435&&r(2,6))return n;if(r(0,5)&&(t[5]===0||t[5]===65535))return n;if(r(0,4)&&t[4]===65535&&t[5]===0)return n;return}
var le=ue(Qao);
function Rgr(e){return Vot(le,e)}
function ips(e){let t=e.toLowerCase().replace(/\.$/,"");return t==="localhost"||t.endsWith(".localhost")}
var Z=30000;
var he=new Set(["connection","keep-alive","proxy-authenticate","proxy-authorization","proxy-connection","te","trailer","transfer-encoding","upgrade"]);
function elo(e){let t=e?.http??process.env.HTTP_PROXY??process.env.http_proxy??void 0,r=e?.https??process.env.HTTPS_PROXY??process.env.https_proxy??t,n=e?.noProxy??process.env.NO_PROXY??process.env.no_proxy??"";if(!t&&!r)return;let o=(c)=>{if(!c)return;let a=/^[a-z][a-z0-9+.-]*:\/\//i.test(c)?c:`http://${c}`;try{let g=new U(a);if(g.protocol!=="http:"&&g.protocol!=="https:"||!g.hostname)throw Error("unsupported scheme or empty host");return g}catch{Pt(`Invalid parent proxy URL, ignoring: ${xe(c)}`,{level:"error"});return}},i=o(t),s=o(r);if(!i&&!s)return;return{httpUrl:i,httpsUrl:s,noProxy:me(n)}}
function me(e){let t={all:!1,suffixes:[],cidr:new de};for(let r of e.split(",")){if(r=r.trim(),!r)continue;if(r==="*"){t.all=!0;continue}if(r.includes("/")){TAt(t.cidr,r);continue}let n=r.toLowerCase(),o=/^\[([^\]]+)\](?::\d+)?$/.exec(n);if(o)n=o[1];if(n.startsWith("*."))n=n.slice(1);if(!T(n)){let i=n.lastIndexOf(":");if(i!==-1&&/^\d+$/.test(n.slice(i+1)))n=n.slice(0,i)}else if(TAt(t.cidr,n))continue;t.suffixes.push(n)}return t}
function yHn(e,t){let r=CAe(t.toLowerCase().replace(/\.$/,""));if(r==="localhost"||Rgr(r))return!0;if(e.noProxy.all)return!0;if(Vot(e.noProxy.cidr,r))return!0;for(let n of e.noProxy.suffixes)if(n.startsWith(".")){if(r===n.slice(1)||r.endsWith(n))return!0}else if(r===n||r.endsWith("."+n))return!0;return!1}
function _Hn(e,t){if(t.isHttps)return e.httpsUrl??e.httpUrl;return e.httpUrl}
function tlo(e){let{destHost:t,destPort:r}=e,n=CAe(t);if(!X9t(n))return Promise.reject(Error(`Invalid destination host for CONNECT: ${JSON.stringify(t)}`));if(!Number.isInteger(r)||r<1||r>65535)return Promise.reject(Error(`Invalid destination port: ${r}`));let o=T(n)===6?`[${n}]:${r}`:`${n}:${r}`;return new Promise((i,s)=>{let c=e.dial(),f=!1,a=(x)=>{if(f)return;f=!0,c.destroy(),s(x)},g=()=>a(Error("Proxy closed during CONNECT handshake"));c.setTimeout(e.timeoutMs??Z,()=>a(Error("CONNECT handshake timed out"))),c.once("error",a),c.once("close",g),c.once(e.readyEvent,()=>{c.write(`CONNECT ${o} HTTP/1.1\r
Host: ${o}\r
`+(e.authHeader?`Proxy-Authorization: ${e.authHeader}\r
`:"")+`\r
`);let x="",C=(y)=>{x+=y.toString("latin1");let S=x.indexOf(`\r
\r
`);if(S===-1){if(x.length>16384)a(Error("CONNECT response header too large"));return}c.pause(),c.removeListener("data",C);let u=x.slice(0,x.indexOf(`\r
`));if(!/^HTTP\/1\.[01] 2\d\d(?:\s|$)/.test(u))return a(Error(`Proxy refused CONNECT: ${u.trim()}`));let l=x.slice(S+4);if(l.length)c.unshift(Buffer.from(l,"latin1"));f=!0,c.setTimeout(0),c.removeListener("error",a),c.removeListener("close",g),i(c)};c.on("data",C)})})}
function xgr(e,t,r){let n=CAe(e.hostname),o=Number(e.port)||(e.protocol==="https:"?443:80),i=e.protocol==="https:";return tlo({destHost:t,destPort:r,authHeader:nlo(e),readyEvent:i?"secureConnect":"connect",dial:()=>i?pe({host:n,port:o,...T(n)?{}:{servername:n}}):V(o,n)})}
function nlo(e){if(!e.username&&!e.password)return;try{let t=`${decodeURIComponent(e.username)}:${decodeURIComponent(e.password)}`;return`Basic ${Buffer.from(t).toString("base64")}`}catch{let t=`${e.username}:${e.password}`;return`Basic ${Buffer.from(t).toString("base64")}`}}
function qot(e){let t=new Set,r=e.connection;if(r)for(let o of String(r).split(","))t.add(o.trim().toLowerCase());let n={};for(let[o,i]of Object.entries(e)){let s=o.toLowerCase();if(!he.has(s)&&!t.has(s))n[o]=i}return n}
function CAe(e){return e.startsWith("[")&&e.endsWith("]")?e.slice(1,-1):e}
function rlo(e){if(!e)return"-";if(!e.username&&!e.password)return e.href;let t=new U(e.href);return t.username="***",t.password="***",t.href}
function xe(e){return e.replace(/\/\/[^@/]*@/,"//***:***@")}
function X9t(e){if(!e||e.length>255)return!1;let t=CAe(e);if(t.includes("%"))return!1;if(T(t))return!0;return/^[A-Za-z0-9._-]+$/.test(t)}
function PM(e){try{let t=CAe(e),r=T(t)===6?`[${t}]`:t,n=CAe(new U(`http://${r}/`).hostname);return T(n)===6?G(n)??n:n.replace(/\.$/,"")}catch{return}}
function Pgr(e,t,r){let n=T(e)===6?`[${e}]`:e;return t===r?n:`${n}:${t}`}
async function Igr(e,t,r,n){let o=e;if(r&&!T(e)){let i=await SHn(e,t,r);if(o=i.remoteAddress??"",i.destroy(),!o)throw Error(`connect ${e}:${t}: no peer address`)}return{host:o,port:t,...n&&!T(e)?{servername:e}:{},agent:!1}}
function SHn(e,t,r,n=Z){return new Promise((o,i)=>{let s=V({port:t,host:e,lookup:r}),c=!1,f=(a)=>{if(c)return;if(c=!0,s.setTimeout(0),a)s.destroy(),i(a);else o(s)};s.setTimeout(n,()=>f(Error("connect timed out"))),s.once("connect",()=>f()),s.once("error",f),s.once("close",()=>f(Error("socket closed before connect")))})}
function J(e){let t=e.indexOf("%");return t===-1?e:e.slice(0,t)}
function yJ(e){if(e.startsWith("[")){let n=e.indexOf("]");if(n===-1)return{hostPattern:e,port:void 0};let o=J(e.slice(1,n)),i=PM(o)??o,s=e.slice(n+1);if(s==="")return{hostPattern:i,port:void 0};let c=Q(s.startsWith(":")?s.slice(1):"");return c===void 0?{hostPattern:e,port:void 0}:{hostPattern:i,port:c}}let t=e.lastIndexOf(":");if(t===-1)return{hostPattern:e,port:void 0};if(e.indexOf(":")!==t){let n=J(e);return{hostPattern:PM(n)??n,port:void 0}}let r=Q(e.slice(t+1));if(r===void 0)return{hostPattern:e,port:void 0};return{hostPattern:e.slice(0,t),port:r}}
function Q(e){if(!/^[1-9][0-9]{0,4}$/.test(e))return;let t=Number(e);return t>65535?void 0:t}
function olo(e){return e.flatMap((t)=>{let{hostPattern:r,port:n}=yJ(t);return q(r)?[{range:r,port:n}]:[]})}
function J9t(e){return yJ(e).hostPattern}
function wqe(e,t){let r=e.toLowerCase();if(t==="*")return!0;if(t.startsWith("*.")){if(q(CAe(r)))return!1;let n=t.substring(2).toLowerCase();return r.endsWith("."+n)}return r===t.toLowerCase()}
function bHn(e,t,r){let{hostPattern:n,port:o}=yJ(r);if(o!==void 0&&o!==t)return!1;return wqe(e,n)}
function RAt(e,t){let r=t.map(J9t);if(!e.startsWith("*."))return r.some((o)=>wqe(e,o));let n=e.slice(2).toLowerCase();return r.some((o)=>{if(!o.startsWith("*."))return!1;let i=o.slice(2).toLowerCase();return n===i||n.endsWith("."+i)})}
export { elo as elo };
export { Pt as Pt };
export { rlo as rlo };
export { TAt as TAt };
export { olo as olo };
export { Qao as Qao };
export { sps as sps };
export { Vot as Vot };
export { ips as ips };
export { Rgr as Rgr };
export { pw as pw };
export { rps as rps };
export { iLe as iLe };
export { Ece as Ece };
export { Kao as Kao };
export { DF as DF };
export { km as km };
export { nd as nd };
export { Yao as Yao };
export { wv as wv };
export { pHn as pHn };
export { fHn as fHn };
export { AAt as AAt };
export { kgr as kgr };
export { Y9t as Y9t };
export { wqe as wqe };
export { hHn as hHn };
export { PM as PM };
export { Igr as Igr };
export { Pgr as Pgr };
export { qot as qot };
export { yHn as yHn };
export { _Hn as _Hn };
export { tlo as tlo };
export { xgr as xgr };
export { SHn as SHn };
export { CAe as CAe };
export { nlo as nlo };
export { X9t as X9t };
export { bHn as bHn };
export { aLe as aLe };
export { J9t as J9t };
export { Jao as Jao };
export { ops as ops };
export { Vao as Vao };
export { Hx as Hx };
export { zot as zot };
export { K9t as K9t };
export { q9t as q9t };
export { CAt as CAt };
export { V9t as V9t };
export { Tgr as Tgr };
export { mHn as mHn };
export { z9t as z9t };
export { zao as zao };
export { Agr as Agr };
export { Xao as Xao };
export { yJ as yJ };
export { lre as lre };
export { RAt as RAt };
