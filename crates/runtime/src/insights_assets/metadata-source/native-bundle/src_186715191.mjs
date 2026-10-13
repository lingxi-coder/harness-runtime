// Extracted source closure for 2.1.295; source module src_186715191.js sha256 c4517a9d59f37b190ad5ee1e81d809f81255a1ab11e60a6db94c92f26ac41e52
import { De, ys, q, Cb } from "./src_186096608.mjs";
import { x, E, zr } from "./src_186056234.mjs";
import { O } from "./src_186477637.mjs";
import { a } from "./src_186481843.mjs";
import { _s } from "./src_186092222.mjs";
import j from "path";
import Mo from "path";
import { posix as T } from "path";
import { readFileSync as G, mkdirSync as Ye, writeFileSync as pe, rmdirSync as Nn, statSync as Ko, createReadStream as Co, createWriteStream as Qr, readFileSync as Eo } from "fs";
import Wr, { totalmem as zo, constants as kr } from "os";
import { t } from "./src_186240709.mjs";
import { i } from "./src_186440090.mjs";
import { S, d } from "./src_186016069.mjs";
import { g, m } from "./src_186441656.mjs";
import { be, A, Me } from "./src_186050226.mjs";
import fe from "process";
import re from "process";
import Go from "process";
import Ur from "process";
import { debuglog as No } from "util";
import { fileURLToPath as qt } from "url";
import qe, { ChildProcess as Jr } from "child_process";
import { setTimeout as wo } from "timers/promises";
import { Buffer as jo } from "buffer";
import { YB } from "./src_186453827.mjs";
var ft=A(function(gi,lt){lt.exports=dt;dt.sync=Yn;var ut=Me("fs");function Xn(e,n){var r=n.pathExt!==void 0?n.pathExt:process.env.PATHEXT;if(!r)return!0;if(r=r.split(";"),r.indexOf("")!==-1)return!0;for(var o=0;o<r.length;o++){var s=r[o].toLowerCase();if(s&&e.substr(-s.length).toLowerCase()===s)return!0}return!1}function ct(e,n,r){if(!e.isSymbolicLink()&&!e.isFile())return!1;return Xn(n,r)}function dt(e,n,r){ut.stat(e,function(o,s){r(o,o?!1:ct(s,e,n))})}function Yn(e,n){return ct(ut.statSync(e),e,n)}});
var yt=A(function(hi,ht){ht.exports=mt;mt.sync=Zn;var pt=Me("fs");function mt(e,n,r){pt.stat(e,function(o,s){r(o,o?!1:gt(s,n))})}function Zn(e,n){return gt(pt.statSync(e),n)}function gt(e,n){return e.isFile()&&Qn(e,n)}function Qn(e,n){var{mode:r,uid:o,gid:s}=e,u=n.uid!==void 0?n.uid:process.getuid&&process.getuid(),c=n.gid!==void 0?n.gid:process.getgid&&process.getgid(),l=parseInt("100",8),f=parseInt("010",8),p=parseInt("001",8),h=l|f,b=r&p||r&f&&s===c||r&l&&o===u||r&h&&u===0;return b}});
var xt=A(function(Si,St){var yi=Me("fs"),Z;if(global.TESTING_WINDOWS)Z=ft();else Z=yt();St.exports=Se;Se.sync=Jn;function Se(e,n,r){if(typeof n==="function")r=n,n={};if(!r){if(typeof Promise!=="function")throw TypeError("callback not provided");return new Promise(function(o,s){Se(e,n||{},function(u,c){if(u)s(u);else o(c)})})}Z(e,n||{},function(o,s){if(o){if(o.code==="EACCES"||n&&n.ignoreErrors)o=null,s=!1}r(o,s)})}function Jn(e,n){try{return Z.sync(e,n||{})}catch(r){if(n&&n.ignoreErrors||r.code==="EACCES")return!1;else throw r}}});
var Tt=A(function(xi,wt){var _=process.env.OSTYPE==="cygwin"||process.env.OSTYPE==="msys",Q=Me("path"),er=_?";":":",xe=xt(),bt=(e)=>Object.assign(Error(`not found: ${e}`),{code:"ENOENT"}),Ct=(e,n)=>{let r=n.colon||er,o=e.match(/\//)||_&&e.match(/\\/)?[""]:[..._?[process.cwd()]:[],...(n.path||process.env.PATH||"").split(r)],s=_?n.pathExt||process.env.PATHEXT||".EXE;.CMD;.BAT;.COM":"",u=_?s.split(r):[""];if(_){if(e.indexOf(".")!==-1&&u[0]!=="")u.unshift("")}return{pathEnv:o,pathExt:u,pathExtExe:s}},Et=(e,n,r)=>{if(typeof n==="function")r=n,n={};if(!n)n={};let{pathEnv:o,pathExt:s,pathExtExe:u}=Ct(e,n),c=[],l=(p)=>new Promise((h,b)=>{if(p===o.length)return n.all&&c.length?h(c):b(bt(e));let y=o[p],w=/^".*"$/.test(y)?y.slice(1,-1):y,C=Q.join(w,e),I=!w&&/^\.[\\\/]/.test(e)?e.slice(0,2)+C:C;h(f(I,p,0))}),f=(p,h,b)=>new Promise((y,w)=>{if(b===s.length)return y(l(h+1));let C=s[b];xe(p+C,{pathExt:u},(I,v)=>{if(!I&&v)if(n.all)c.push(p+C);else return y(p+C);return y(f(p,h,b+1))})});return r?l(0).then((p)=>r(null,p),r):l(0)},tr=(e,n)=>{n=n||{};let{pathEnv:r,pathExt:o,pathExtExe:s}=Ct(e,n),u=[];for(let c=0;c<r.length;c++){let l=r[c],f=/^".*"$/.test(l)?l.slice(1,-1):l,p=Q.join(f,e),h=!f&&/^\.[\\\/]/.test(e)?e.slice(0,2)+p:p;for(let b=0;b<o.length;b++){let y=h+o[b];try{if(xe.sync(y,{pathExt:s}))if(n.all)u.push(y);else return y}catch(w){}}}if(n.all&&u.length)return u;if(n.nothrow)return null;throw bt(e)};wt.exports=Et;Et.sync=tr});
var It=A(function(bi,Ce){var Pt=(e={})=>{let n=e.env||process.env;if((e.platform||"darwin")!=="win32")return"PATH";return Object.keys(n).reverse().find((o)=>o.toUpperCase()==="PATH")||"Path"};Ce.exports=Pt;Ce.exports.default=Pt});
var Rt=A(function(Ci,At){var J=Me("path"),Ot=Tt(),nr=It();function vt(e,n){let r=e.options.env||process.env,o=process.cwd(),s=e.options.cwd!=null,u=s&&process.chdir!==void 0&&!process.chdir.disabled;if(u)try{process.chdir(e.options.cwd)}catch(l){}let c;try{c=Ot.sync(e.command,{path:r[nr({env:r})],pathExt:n?J.delimiter:void 0})}catch(l){}finally{if(u)try{process.chdir(o)}catch(l){let f=l!==null&&typeof l==="object"&&typeof l.code==="string"?l.code:"";if(!(/^E[A-Z0-9]+$/.test(f)||f==="UNKNOWN"||f.startsWith("Unknown system error")))throw l}}if(c)c=J.resolve(s?e.options.cwd:"",c);return c}function rr(e){return vt(e)||vt(e,!0)}At.exports=rr});
var _t=A(function(sr,we){var Ee=/([()\][%!^"`<>&|;, *?])/g;function or(e){return e=e.replace(Ee,"^$1"),e}function ir(e,n){if(e=`${e}`,e=e.replace(/(?=(\\+?)?)\1"/g,"$1$1\\\""),e=e.replace(/(?=(\\+?)?)\1$/,"$1$1"),e=`"${e}"`,e=e.replace(Ee,"^$1"),n)e=e.replace(Ee,"^$1");return e}sr.command=or;sr.argument=ir});
var kt=A(function(Ei,$t){$t.exports=/^#!(.*)/});
var Nt=A(function(wi,Ft){var cr=kt();Ft.exports=(e="")=>{let n=e.match(cr);if(!n)return null;let[r,o]=n[0].replace(/#! ?/,"").split(" "),s=r.split("/").pop();if(s==="env")return o;return o?`${s} ${o}`:s}});
var Lt=A(function(Ti,Gt){var k=Me("fs"),dr=Nt();function lr(e){let r=Buffer.alloc(150),o;try{o=k.openSync(e,"r"),k.readSync(o,r,0,150,0),k.closeSync(o)}catch(s){}return dr(r.toString())}Gt.exports=lr});
var Ut=A(function(Pi,Dt){var Mt=Me("path"),jt=Rt(),ee=_t(),fr=Lt(),pr=/\.(?:com|exe)$/i,mr=/node_modules[\\/].bin[\\/][^\\/]+\.cmd$/i;function gr(e){e.file=jt(e);let n=e.file&&fr(e.file);if(n)return e.args.unshift(e.file),e.command=n,jt(e);return e.file}function hr(e){return e}function yr(e,n,r){if(n&&!Array.isArray(n))r=n,n=null;n=n?n.slice(0):[],r=Object.assign({},r);let o={command:e,args:n,options:r,file:void 0,original:{command:e,args:n}};return r.shell?o:hr(o)}Dt.exports=yr});
var zt=A(function(Ii,Kt){function Sr(e,n){return Object.assign(Error(`${n} ${e.command} ENOENT`),{code:"ENOENT",errno:"ENOENT",syscall:`${n} ${e.command}`,path:e.command,spawnargs:e.args})}function xr(e,n){return}function Bt(e,n){return null}function br(e,n){return null}Kt.exports={hookChildProcess:xr,verifyENOENT:Bt,verifyENOENTSync:br,notFoundError:Sr}});
var YWn=A(function(vi,F){var te=Me("child_process"),Te=Ut(),L=zt();function Wt(e,n,r){let o=Te(e,n,r),s=te.spawn(o.command,o.args,o.options);return L.hookChildProcess(s,o),s}function Cr(e,n,r){let o=Te(e,n,r),s=te.spawnSync(o.command,o.args,o.options);return s.error=s.error||L.verifyENOENTSync(s.status,o),s}F.exports=Wt;F.exports.spawn=Wt;F.exports.sync=Cr;F.exports._parse=Te;F.exports._enoent=L});
var Sn=A(function(Os,yn){var{PassThrough:bo}=Me("stream");yn.exports=function(){var e=[],n=new bo({objectMode:!0});return n.setMaxListeners(0),n.add=r,n.isEmpty=o,n.on("unpipe",s),Array.prototype.slice.call(arguments).forEach(r),n;function r(u){if(Array.isArray(u))return u.forEach(r),this;return e.push(u),u.once("end",s.bind(null,u)),u.once("error",n.emit.bind(n,"error")),u.pipe(n,{end:!1}),this}function o(){return e.length==0}function s(u){if(e=e.filter(function(c){return c!==u}),!e.length&&n.readable)n.end()}}});
var R=[];
var Y=(e)=>!!e&&typeof e==="object"&&typeof e.removeListener==="function"&&typeof e.emit==="function"&&typeof e.reallyExit==="function"&&typeof e.listeners==="function"&&typeof e.kill==="function"&&typeof e.pid==="number"&&typeof e.on==="function";
var me=Symbol.for("signal-exit emitter");
var ge=globalThis;
var Hn=Object.defineProperty.bind(Object);
class it{emitted={afterExit:!1,exit:!1};listeners={afterExit:[],exit:[]};count=0;id=Math.random();constructor(){if(ge[me])return ge[me];Hn(ge,me,{value:this,writable:!1,enumerable:!1,configurable:!1})}on(e,n){this.listeners[e].push(n)}removeListener(e,n){let r=this.listeners[e],o=r.indexOf(n);if(o===-1)return;if(o===0&&r.length===1)r.length=0;else r.splice(o,1)}emit(e,n,r){if(this.emitted[e])return!1;this.emitted[e]=!0;let o=!1;for(let s of this.listeners[e])o=s(n,r)===!0||o;if(e==="exit")o=this.emit("afterExit",n,r)||o;return o}}
class ye{}
var Vn=(e)=>({onExit(n,r){return e.onExit(n,r)},load(){return e.load()},unload(){return e.unload()}});
class st extends ye{onExit(){return()=>{}}load(){}unload(){}}
class at extends ye{#s=he.platform==="win32"?"SIGINT":"SIGHUP";#t=new it;#e;#o;#i;#r={};#n=!1;constructor(e){super();this.#e=e,this.#r={};for(let n of R)this.#r[n]=()=>{let r=this.#e.listeners(n),{count:o}=this.#t,s=e;if(typeof s.__signal_exit_emitter__==="object"&&typeof s.__signal_exit_emitter__.count==="number")o+=s.__signal_exit_emitter__.count;if(r.length===o){this.unload();let u=this.#t.emit("exit",null,n),c=n==="SIGHUP"?this.#s:n;if(!u)e.kill(e.pid,c)}};this.#i=e.reallyExit,this.#o=e.emit}onExit(e,n){if(!Y(this.#e))return()=>{};if(this.#n===!1)this.load();let r=n?.alwaysLast?"afterExit":"exit";return this.#t.on(r,e),()=>{if(this.#t.removeListener(r,e),this.#t.listeners.exit.length===0&&this.#t.listeners.afterExit.length===0)this.unload()}}load(){if(this.#n)return;this.#n=!0,this.#t.count+=1;for(let e of R)try{let n=this.#r[e];if(n)this.#e.on(e,n)}catch(n){}this.#e.emit=(e,...n)=>this.#u(e,...n),this.#e.reallyExit=(e)=>this.#a(e)}unload(){if(!this.#n)return;this.#n=!1,R.forEach((e)=>{let n=this.#r[e];if(!n)throw Error("Listener not defined for signal: "+e);try{this.#e.removeListener(e,n)}catch(r){}}),this.#e.emit=this.#o,this.#e.reallyExit=this.#i,this.#t.count-=1}#a(e){if(!Y(this.#e))return 0;return this.#e.exitCode=e||0,this.#t.emit("exit",this.#e.exitCode,null),this.#i.call(this.#e,this.#e.exitCode)}#u(e,...n){let r=this.#o;if(e==="exit"&&Y(this.#e)){if(typeof n[0]==="number")this.#e.exitCode=n[0];let o=r.call(this.#e,e,...n);return this.#t.emit("exit",this.#e.exitCode,null),o}else return r.call(this.#e,e,...n)}}
var he=globalThis.process;
var {onExit:dY,load:fi,unload:pi}=Vn(Y(he)?new at(he):new st);
var _n=be(YWn(),1);
function Pe(e){let n=typeof e==="string"?`
`:`
`.charCodeAt(),r=typeof e==="string"?"\r":"\r".charCodeAt();if(e[e.length-1]===n)e=e.slice(0,-1);if(e[e.length-1]===r)e=e.slice(0,-1);return e}
function ne(e={}){let{env:n=process.env,platform:r="darwin"}=e;if(r!=="win32")return"PATH";return Object.keys(n).reverse().find((o)=>o.toUpperCase()==="PATH")||"Path"}
var Er=({cwd:e=re.cwd(),path:n=re.env[ne()],preferLocal:r=!0,execPath:o=re.execPath,addExecPath:s=!0}={})=>{let u=e instanceof URL?qt(e):e,c=j.resolve(u),l=[];if(r)wr(l,c);if(s)Tr(l,o,c);return[...l,n].join(j.delimiter)};
var wr=(e,n)=>{let r;while(r!==n)e.push(j.join(n,"node_modules/.bin")),r=n,n=j.resolve(n,"..")};
var Tr=(e,n,r)=>{let o=n instanceof URL?qt(n):n;e.push(j.resolve(r,o,".."))};
var Ht=({env:e=re.env,...n}={})=>{e={...e};let r=ne({env:e});return n.path=e[r],e[r]=Er(n),e};
var Pr=(e,n,r,o)=>{if(r==="length"||r==="prototype")return;if(r==="arguments"||r==="caller")return;let s=Object.getOwnPropertyDescriptor(e,r),u=Object.getOwnPropertyDescriptor(n,r);if(!Ir(s,u)&&o)return;Object.defineProperty(e,r,u)};
var Ir=function(e,n){return e===void 0||e.configurable||e.writable===n.writable&&e.enumerable===n.enumerable&&e.configurable===n.configurable&&(e.writable||e.value===n.value)};
var vr=(e,n)=>{let r=Object.getPrototypeOf(n);if(r===Object.getPrototypeOf(e))return;Object.setPrototypeOf(e,r)};
var Or=(e,n)=>`/* Wrapped ${e}*/
${n}`;
var Ar=Object.getOwnPropertyDescriptor(Function.prototype,"toString");
var Rr=Object.getOwnPropertyDescriptor(Function.prototype.toString,"name");
var _r=(e,n,r)=>{let o=r===""?"":`with ${r.trim()}() `,s=Or.bind(null,o,n.toString());Object.defineProperty(s,"name",Rr),Object.defineProperty(e,"toString",{...Ar,value:s})};
function Ie(e,n,{ignoreNonConfigurable:r=!1}={}){let{name:o}=e;for(let s of Reflect.ownKeys(n))Pr(e,n,s,r);return vr(e,n),_r(e,n,o),e}
var oe=new WeakMap;
var Vt=(e,n={})=>{if(typeof e!=="function")throw TypeError("Expected a function");let r,o=0,s=e.displayName||e.name||"<anonymous>",u=function(...c){if(oe.set(u,++o),o===1)r=e.apply(this,c),e=null;else if(n.throw===!0)throw Error(`Function \`${s}\` can only be called once`);return r};return Ie(u,e),oe.set(u,o),u};
var Xt=Vt;
var Yt=()=>{let e=ve-Zt+1;return Array.from({length:e},$r)};
var $r=(e,n)=>({name:`SIGRT${n+1}`,number:Zt+n,action:"terminate",description:"Application-specific signal (realtime)",standard:"posix"});
var Zt=34;
var ve=64;
var Qt=[{name:"SIGHUP",number:1,action:"terminate",description:"Terminal closed",standard:"posix"},{name:"SIGINT",number:2,action:"terminate",description:"User interruption with CTRL-C",standard:"ansi"},{name:"SIGQUIT",number:3,action:"core",description:"User interruption with CTRL-\\",standard:"posix"},{name:"SIGILL",number:4,action:"core",description:"Invalid machine instruction",standard:"ansi"},{name:"SIGTRAP",number:5,action:"core",description:"Debugger breakpoint",standard:"posix"},{name:"SIGABRT",number:6,action:"core",description:"Aborted",standard:"ansi"},{name:"SIGIOT",number:6,action:"core",description:"Aborted",standard:"bsd"},{name:"SIGBUS",number:7,action:"core",description:"Bus error due to misaligned, non-existing address or paging error",standard:"bsd"},{name:"SIGEMT",number:7,action:"terminate",description:"Command should be emulated but is not implemented",standard:"other"},{name:"SIGFPE",number:8,action:"core",description:"Floating point arithmetic error",standard:"ansi"},{name:"SIGKILL",number:9,action:"terminate",description:"Forced termination",standard:"posix",forced:!0},{name:"SIGUSR1",number:10,action:"terminate",description:"Application-specific signal",standard:"posix"},{name:"SIGSEGV",number:11,action:"core",description:"Segmentation fault",standard:"ansi"},{name:"SIGUSR2",number:12,action:"terminate",description:"Application-specific signal",standard:"posix"},{name:"SIGPIPE",number:13,action:"terminate",description:"Broken pipe or socket",standard:"posix"},{name:"SIGALRM",number:14,action:"terminate",description:"Timeout or timer",standard:"posix"},{name:"SIGTERM",number:15,action:"terminate",description:"Termination",standard:"ansi"},{name:"SIGSTKFLT",number:16,action:"terminate",description:"Stack is empty or overflowed",standard:"other"},{name:"SIGCHLD",number:17,action:"ignore",description:"Child process terminated, paused or unpaused",standard:"posix"},{name:"SIGCLD",number:17,action:"ignore",description:"Child process terminated, paused or unpaused",standard:"other"},{name:"SIGCONT",number:18,action:"unpause",description:"Unpaused",standard:"posix",forced:!0},{name:"SIGSTOP",number:19,action:"pause",description:"Paused",standard:"posix",forced:!0},{name:"SIGTSTP",number:20,action:"pause",description:'Paused using CTRL-Z or "suspend"',standard:"posix"},{name:"SIGTTIN",number:21,action:"pause",description:"Background process cannot read terminal input",standard:"posix"},{name:"SIGBREAK",number:21,action:"terminate",description:"User interruption with CTRL-BREAK",standard:"other"},{name:"SIGTTOU",number:22,action:"pause",description:"Background process cannot write to terminal output",standard:"posix"},{name:"SIGURG",number:23,action:"ignore",description:"Socket received out-of-band data",standard:"bsd"},{name:"SIGXCPU",number:24,action:"core",description:"Process timed out",standard:"bsd"},{name:"SIGXFSZ",number:25,action:"core",description:"File too big",standard:"bsd"},{name:"SIGVTALRM",number:26,action:"terminate",description:"Timeout or timer",standard:"bsd"},{name:"SIGPROF",number:27,action:"terminate",description:"Timeout or timer",standard:"bsd"},{name:"SIGWINCH",number:28,action:"ignore",description:"Terminal window size changed",standard:"bsd"},{name:"SIGIO",number:29,action:"terminate",description:"I/O is available",standard:"other"},{name:"SIGPOLL",number:29,action:"terminate",description:"Watched event",standard:"other"},{name:"SIGINFO",number:29,action:"ignore",description:"Request for process information",standard:"other"},{name:"SIGPWR",number:30,action:"terminate",description:"Device running out of power",standard:"systemv"},{name:"SIGSYS",number:31,action:"core",description:"Invalid system call",standard:"other"},{name:"SIGUNUSED",number:31,action:"terminate",description:"Invalid system call",standard:"other"}];
var Oe=()=>{let e=Yt();return[...Qt,...e].map(Fr)};
var Fr=({name:e,number:n,description:r,action:o,forced:s=!1,standard:u})=>{let{signals:{[e]:c}}=kr,l=c!==void 0;return{name:e,number:l?c:n,description:r,supported:l,action:o,forced:s,standard:u}};
var Gr=()=>{let e=Oe();return Object.fromEntries(e.map(Lr))};
var Lr=({name:e,number:n,description:r,supported:o,action:s,forced:u,standard:c})=>[e,{name:e,number:n,description:r,supported:o,action:s,forced:u,standard:c}];
var Jt=Gr();
var Br=({timedOut:e,timeout:n,errorCode:r,signal:o,signalDescription:s,exitCode:u,isCanceled:c})=>{if(e)return`timed out after ${n} milliseconds`;if(c)return"was canceled";if(r!==void 0)return`failed with ${r}`;if(o!==void 0)return`was killed with ${o} (${s})`;if(u!==void 0)return`failed with exit code ${u}`;return"failed"};
var M=({stdout:e,stderr:n,all:r,error:o,signal:s,exitCode:u,command:c,escapedCommand:l,timedOut:f,isCanceled:p,killed:h,parsed:{options:{timeout:b,cwd:y=Ur.cwd()}}})=>{u=u===null?void 0:u,s=s===null?void 0:s;let w=s===void 0?void 0:Jt[s].description,C=o&&o.code,v=`Command ${Br({timedOut:f,timeout:b,errorCode:C,signal:s,signalDescription:w,exitCode:u,isCanceled:p})}: ${c}`,H=Object.prototype.toString.call(o)==="[object Error]",V=H?`${v}
${o.message}`:v,X=[V,n,e].filter(Boolean).join(`
`);if(H)o.originalMessage=o.message,o.message=X;else o=Error(X);if(o.shortMessage=V,o.command=c,o.escapedCommand=l,o.exitCode=u,o.signal=s,o.signalDescription=w,o.stdout=e,o.stderr=n,o.cwd=y,r!==void 0)o.all=r;if("bufferedData"in o)delete o.bufferedData;return o.failed=!0,o.timedOut=Boolean(f),o.isCanceled=p,o.killed=h&&!f,o};
var ie=["stdin","stdout","stderr"];
var Kr=(e)=>ie.some((n)=>e[n]!==void 0);
var en=(e)=>{if(!e)return;let{stdio:n}=e;if(n===void 0)return ie.map((o)=>e[o]);if(Kr(e))throw Error(`It's not possible to provide \`stdio\` in combination with one of ${ie.map((o)=>`\`${o}\``).join(", ")}`);if(typeof n==="string")return n;if(!Array.isArray(n))throw TypeError(`Expected \`stdio\` to be of type \`string\` or \`Array\`, got \`${typeof n}\``);let r=Math.max(n.length,ie.length);return Array.from({length:r},(o,s)=>n[s])};
var qr=5000;
var tn=(e,n="SIGTERM",r={})=>{let o=e(n);return Hr(e,n,r,o),o};
var Hr=(e,n,r,o)=>{if(!Vr(n,r,o))return;let s=Yr(r),u=setTimeout(()=>{e("SIGKILL")},s);if(u.unref)u.unref()};
var Vr=(e,{forceKillAfterTimeout:n},r)=>Xr(e)&&n!==!1&&r;
var Xr=(e)=>e===Wr.constants.signals.SIGTERM||typeof e==="string"&&e.toUpperCase()==="SIGTERM";
var Yr=({forceKillAfterTimeout:e=!0})=>{if(e===!0)return qr;if(!Number.isFinite(e)||e<0)throw TypeError(`Expected the \`forceKillAfterTimeout\` option to be a non-negative integer, got \`${e}\` (${typeof e})`);return e};
var nn=(e,n)=>{if(e.kill())n.isCanceled=!0};
var Zr=(e,n,r)=>{e.kill(n),r(Object.assign(Error("Timed out"),{timedOut:!0,signal:n}))};
var rn=(e,{timeout:n,killSignal:r="SIGTERM"},o)=>{if(n===0||n===void 0)return o;let s,u=new Promise((l,f)=>{s=setTimeout(()=>{Zr(e,r,f)},n)}),c=o.finally(()=>{clearTimeout(s)});return Promise.race([u,c])};
var on=({timeout:e})=>{if(e!==void 0&&(!Number.isFinite(e)||e<0))throw TypeError(`Expected the \`timeout\` option to be a non-negative integer, got \`${e}\` (${typeof e})`)};
var sn=async(e,{cleanup:n,detached:r},o)=>{if(!n||r)return o;let s=dY(()=>{e.kill()});return o.finally(()=>{s()})};
function se(e){return e!==null&&typeof e==="object"&&typeof e.pipe==="function"}
function Ae(e){return se(e)&&e.writable!==!1&&typeof e._write==="function"&&typeof e._writableState==="object"}
var eo=(e)=>e instanceof Jr&&typeof e.then==="function";
var Re=(e,n,r)=>{if(typeof r==="string")return e[n].pipe(Qr(r)),e;if(Ae(r))return e[n].pipe(r),e;if(!eo(r))throw TypeError("The second argument must be a string, a stream or an Execa child process.");if(!Ae(r.stdin))throw TypeError("The target child process's stdin must be available.");return e[n].pipe(r.stdin),r};
var an=(e)=>{if(e.stdout!==null)e.pipeStdout=Re.bind(void 0,e,"stdout");if(e.stderr!==null)e.pipeStderr=Re.bind(void 0,e,"stderr");if(e.all!==void 0)e.pipeAll=Re.bind(void 0,e,"all")};
var D=async(e,{init:n,convertChunk:r,getSize:o,truncateChunk:s,addChunk:u,getFinalChunk:c,finalize:l},{maxBuffer:f=Number.POSITIVE_INFINITY}={})=>{if(!no(e))throw Error("The first argument must be a Readable, a ReadableStream, or an async iterable.");let p=n();p.length=0;try{for await(let h of e){let b=ro(h),y=r[b](h,p);dn({convertedChunk:y,state:p,getSize:o,truncateChunk:s,addChunk:u,maxBuffer:f})}return to({state:p,convertChunk:r,getSize:o,truncateChunk:s,addChunk:u,getFinalChunk:c,maxBuffer:f}),l(p)}catch(h){throw h.bufferedData=l(p),h}};
var to=({state:e,getSize:n,truncateChunk:r,addChunk:o,getFinalChunk:s,maxBuffer:u})=>{let c=s(e);if(c!==void 0)dn({convertedChunk:c,state:e,getSize:n,truncateChunk:r,addChunk:o,maxBuffer:u})};
var dn=({convertedChunk:e,state:n,getSize:r,truncateChunk:o,addChunk:s,maxBuffer:u})=>{let c=r(e),l=n.length+c;if(l<=u){un(e,n,s,l);return}let f=o(e,u-n.length);if(f!==void 0)un(f,n,s,u);throw new _e};
var un=(e,n,r,o)=>{n.contents=r(e,n,o),n.length=o};
var no=(e)=>typeof e==="object"&&e!==null&&typeof e[Symbol.asyncIterator]==="function";
var ro=(e)=>{let n=typeof e;if(n==="string")return"string";if(n!=="object"||e===null)return"others";if(globalThis.Buffer?.isBuffer(e))return"buffer";let r=cn.call(e);if(r==="[object ArrayBuffer]")return"arrayBuffer";if(r==="[object DataView]")return"dataView";if(Number.isInteger(e.byteLength)&&Number.isInteger(e.byteOffset)&&cn.call(e.buffer)==="[object ArrayBuffer]")return"typedArray";return"others"};
var {toString:cn}=Object.prototype;
class _e extends Error{name="MaxBufferError";constructor(){super("maxBuffer exceeded")}}
var $e=(e)=>e;
var ke=()=>{return};
var Fe=({contents:e})=>e;
var ae=(e)=>{throw Error(`Streams in object mode are not supported: ${String(e)}`)};
var ue=(e)=>e.length;
async function Ne(e,n){return D(e,mo,n)}
var io=()=>({contents:new ArrayBuffer(0)});
var so=(e)=>ao.encode(e);
var ao=new TextEncoder;
var ln=(e)=>new Uint8Array(e);
var fn=(e)=>new Uint8Array(e.buffer,e.byteOffset,e.byteLength);
var uo=(e,n)=>e.slice(0,n);
var co=(e,{contents:n,length:r},o)=>{let s=gn()?fo(n,o):lo(n,o);return new Uint8Array(s).set(e,r),s};
var lo=(e,n)=>{if(n<=e.byteLength)return e;let r=new ArrayBuffer(mn(n));return new Uint8Array(r).set(new Uint8Array(e),0),r};
var fo=(e,n)=>{if(n<=e.maxByteLength)return e.resize(n),e;let r=new ArrayBuffer(n,{maxByteLength:mn(n)});return new Uint8Array(r).set(new Uint8Array(e),0),r};
var mn=(e)=>pn**Math.ceil(Math.log(e)/Math.log(pn));
var pn=2;
var po=({contents:e,length:n})=>gn()?e:e.slice(0,n);
var gn=()=>("resize"in ArrayBuffer.prototype);
var mo={init:io,convertChunk:{string:so,buffer:ln,arrayBuffer:ln,dataView:fn,typedArray:fn,others:ae},getSize:ue,truncateChunk:uo,addChunk:co,getFinalChunk:ke,finalize:po};
async function ce(e,n){if(!("Buffer"in globalThis))throw Error("getStreamAsBuffer() is only supported in Node.js");try{return hn(await Ne(e,n))}catch(r){if(r.bufferedData!==void 0)r.bufferedData=hn(r.bufferedData);throw r}}
var hn=(e)=>globalThis.Buffer.from(e);
async function Ge(e,n){return D(e,xo,n)}
var go=()=>({contents:"",textDecoder:new TextDecoder});
var de=(e,{textDecoder:n})=>n.decode(e,{stream:!0});
var ho=(e,{contents:n})=>n+e;
var yo=(e,n)=>e.slice(0,n);
var So=({textDecoder:e})=>{let n=e.decode();return n===""?void 0:n};
var xo={init:go,convertChunk:{string:$e,buffer:de,arrayBuffer:de,dataView:de,typedArray:de,others:ae},getSize:ue,truncateChunk:yo,addChunk:ho,getFinalChunk:So,finalize:Fe};
var xn=be(Sn(),1);
var bn=(e)=>{if(e!==void 0)throw TypeError("The `input` and `inputFile` options cannot be both set.")};
var To=({input:e,inputFile:n})=>{if(typeof n!=="string")return e;return bn(e),Eo(n)};
var Cn=(e)=>{let n=To(e);if(se(n))throw TypeError("The `input` option cannot be a stream in sync mode");return n};
var Po=({input:e,inputFile:n})=>{if(typeof n!=="string")return e;return bn(e),Co(n)};
var En=(e,n)=>{let r=Po(n);if(r===void 0)return;if(se(r))r.pipe(e.stdin);else e.stdin.end(r)};
var wn=(e,{all:n})=>{if(!n||!e.stdout&&!e.stderr)return;let r=xn.default();if(e.stdout)r.add(e.stdout);if(e.stderr)r.add(e.stderr);return r};
var Le=async(e,n)=>{if(!e||n===void 0)return;await wo(0),e.destroy();try{return await n}catch(r){return r.bufferedData}};
var je=(e,{encoding:n,buffer:r,maxBuffer:o})=>{if(!e||!r)return;if(n==="utf8"||n==="utf-8")return Ge(e,{maxBuffer:o});if(n===null||n==="buffer")return ce(e,{maxBuffer:o});return Io(e,o,n)};
var Io=async(e,n,r)=>(await ce(e,{maxBuffer:n})).toString(r);
var Tn=async({stdout:e,stderr:n,all:r},{encoding:o,buffer:s,maxBuffer:u},c)=>{let l=je(e,{encoding:o,buffer:s,maxBuffer:u}),f=je(n,{encoding:o,buffer:s,maxBuffer:u}),p=je(r,{encoding:o,buffer:s,maxBuffer:u*2});try{return await Promise.all([c,l,f,p])}catch(h){return Promise.all([{error:h,signal:h.signal,timedOut:h.timedOut},Le(e,l),Le(n,f),Le(r,p)])}};
var vo=(async()=>{})().constructor.prototype;
var Oo=["then","catch","finally"].map((e)=>[e,Reflect.getOwnPropertyDescriptor(vo,e)]);
var Ue=(e,n)=>{for(let[r,o]of Oo){let s=typeof n==="function"?(...u)=>Reflect.apply(o.value,n(),u):o.value.bind(n);Reflect.defineProperty(e,r,{...o,value:s})}};
var Pn=(e)=>new Promise((n,r)=>{if(e.on("exit",(o,s)=>{n({exitCode:o,signal:s})}),e.on("error",(o)=>{r(o)}),e.stdin)e.stdin.on("error",(o)=>{r(o)})});
var On=(e,n=[])=>{if(!Array.isArray(n))return[e];return[e,...n]};
var _o=/^[\w.-]+$/;
var $o=(e)=>{if(typeof e!=="string"||_o.test(e))return e;return`"${e.replaceAll('"',"\\\"")}"`};
var Be=(e,n)=>On(e,n).join(" ");
var Ke=(e,n)=>On(e,n).map((r)=>$o(r)).join(" ");
var An=No("execa").enabled;
var le=(e,n)=>String(e).padStart(n,"0");
var Lo=()=>{let e=new Date;return`${le(e.getHours(),2)}:${le(e.getMinutes(),2)}:${le(e.getSeconds(),2)}.${le(e.getMilliseconds(),3)}`};
var We=(e,{verbose:n})=>{if(!n)return;Go.stderr.write(`[${Lo()}] ${e}
`)};
var Do=1e8;
var Uo=({env:e,extendEnv:n,preferLocal:r,localDir:o,execPath:s})=>{let u=n?{...fe.env,...e}:e;if(r)return Ht({env:u,cwd:o,execPath:s});return u};
var $n=(e,n,r={})=>{let o=_n.default._parse(e,n,r);if(e=o.command,n=o.args,r=o.options,r={maxBuffer:Do,buffer:!0,stripFinalNewline:!0,extendEnv:!0,preferLocal:!1,localDir:r.cwd||fe.cwd(),execPath:fe.execPath,encoding:"utf8",reject:!0,cleanup:!0,all:!1,windowsHide:!0,verbose:An,...r},r.env=Uo(r),r.stdio=en(r),fe.platform==="win32"&&Mo.basename(e,".exe")==="cmd")n.unshift("/q");return{file:e,args:n,options:r,parsed:o}};
var U=(e,n,r)=>{if(typeof n!=="string"&&!jo.isBuffer(n))return r===void 0?void 0:"";if(e.stripFinalNewline)return Pe(n);return n};
function hut(e,n,r){let o=$n(e,n,r),s=Be(e,n),u=Ke(e,n);We(u,o.options),on(o.options);let c;try{c=qe.spawn(o.file,o.args,o.options)}catch(w){let C=new qe.ChildProcess,I=Promise.reject(M({error:w,stdout:"",stderr:"",all:"",command:s,escapedCommand:u,parsed:o,timedOut:!1,isCanceled:!1,killed:!1}));return Ue(C,I),C}let l=Pn(c),f=rn(c,o.options,l),p=sn(c,o.options,f),h={isCanceled:!1};c.kill=tn.bind(null,c.kill.bind(c)),c.cancel=nn.bind(null,c,h);let y=Xt(async()=>{let[{error:w,exitCode:C,signal:I,timedOut:v},H,V,X]=await Tn(c,o.options,p),tt=U(o.options,H),nt=U(o.options,V),rt=U(o.options,X);if(w||C!==0||I!==null){let ot=M({error:w,exitCode:C,signal:I,stdout:tt,stderr:nt,all:rt,command:s,escapedCommand:u,parsed:o,timedOut:v,isCanceled:h.isCanceled||(o.options.signal?o.options.signal.aborted:!1),killed:c.killed});if(!o.options.reject)return ot;throw ot}return{command:s,escapedCommand:u,exitCode:0,stdout:tt,stderr:nt,all:rt,failed:!1,timedOut:!1,isCanceled:!1,killed:!1}});return En(c,o.options),c.all=wn(c,o.options),an(c),Ue(c,y),c}
function B(e,n,r){let o=$n(e,n,r),s=Be(e,n),u=Ke(e,n);We(u,o.options);let c=Cn(o.options),l;try{l=qe.spawnSync(o.file,o.args,{...o.options,input:c})}catch(h){throw M({error:h,stdout:"",stderr:"",all:"",command:s,escapedCommand:u,parsed:o,timedOut:!1,isCanceled:!1,killed:!1})}let f=U(o.options,l.stdout,l.error),p=U(o.options,l.stderr,l.error);if(l.error||l.status!==0||l.signal!==null){let h=M({stdout:f,stderr:p,error:l.error,signal:l.signal,exitCode:l.status,command:s,escapedCommand:u,parsed:o,timedOut:l.error&&l.error.code==="ETIMEDOUT",isCanceled:!1,killed:l.signal!==null});if(!o.options.reject)return h;throw h}return{command:s,escapedCommand:u,exitCode:0,stdout:f,stderr:p,failed:!1,timedOut:!1,isCanceled:!1,killed:!1}}
var vHt="Programs are unavailable in this session; the command was not run.";
function tG(){return De()}
function $Ue(){if(tG())throw new x(vHt,"a program start was refused in a diskless session")}
function He(e){let n;for(let r of e.split(`
`)){let[o,s,...u]=r.split(":");if(s===void 0||u.length===0)continue;let c=u.join(":");if(s.split(",").includes("memory"))return{mountRoot:"/sys/fs/cgroup/memory",path:c,v2:!1};if(o==="0"&&s==="")n=c}return n===void 0?void 0:{mountRoot:"/sys/fs/cgroup",path:n,v2:!0}}
var Ve="claude-code-bash";
var Gn=1073741824;
var qo=Gn;
var Xe=["mcp","lsp","hooks","plugin","tmux","helper","agent"];
class Ln{reader=null;subscribeRefresh=null;register(e){let n=this.reader;return this.reader=e,n}read(e,n){return this.reader?this.reader(e,n):n}}
var N=new Ln;
class jn{dir=void 0;layout=void 0;limit=0;activatedClasses=new Set;pendingPids=new Map;pendingUnsubscribe=void 0;oomKillsSeen=void 0}
var P=new q(()=>new jn);
function Ze(){let e=ys(P);if(e.dir!==void 0)return e.dir??void 0;if(Cb()){e.dir=null;return}let n=O();if(n!=="linux"&&n!=="wsl"){e.dir=null;return}let r=a.CLAUDE_CODE_TOOL_MEMORY_LIMIT?.trim().toLowerCase();if(r&&(_s(r)||r==="none")){e.dir=null;return}let o=ci(r);if(o===void 0&&N.read("tengu_tool_memory_cgroup",!1)!==!0)return;try{let s=oi(G("/proc/self/cgroup","utf8"));if(!s)throw Error("no memory cgroup hierarchy");let u=ui(o,zo());if(u===void 0){e.dir=null,t("tool cgroup: disabled (host too small for the default cap)"),i("tengu_tool_cgroup",{status:S("host_too_small")});return}zn(s,u),e.dir=s.dir,e.layout=s,e.limit=u,t(`tool cgroup: ${s.dir} ${s.reuse?"(nested: already capped, reusing our own)":`limit=${u}`}`),g("shell_memory_cgroup"),i("tengu_tool_cgroup",s.reuse?{status:S("nested")}:{status:S("enabled"),limit_bytes:u})}catch(s){e.dir=null,t(`tool cgroup: disabled (${E(s)??s})`),m("shell_memory_cgroup",zr(s)??"no_hierarchy"),i("tengu_tool_cgroup",{status:S("disabled")})}return e.dir??void 0}
function K(e){if(Ze()===void 0)return;if(e!=="shell"&&Mn().has(e))return;let n=ys(P),r=Ho(n);if(r===void 0)return;if(Bn(n),!n.activatedClasses.has(e))n.activatedClasses.add(e),i("tengu_tool_cgroup",{status:S("class_enabled"),class:d(e)});return r}
function Ho(e,n={exists:Vo,mkdirSync:Ye,writeFileSync:pe,rmdirSync:Nn}){if(e.dir===void 0||e.dir===null)return;try{if(n.exists(e.dir))return e.dir;if(e.layout===void 0||e.layout.reuse)throw Error("enclosing tool cgroup is gone");return zn(e.layout,e.limit,n),t(`tool cgroup: ${e.dir} vanished; re-created`),i("tengu_tool_cgroup",{status:S("recreated")}),e.dir}catch(r){e.dir=null,t(`tool cgroup: disabled (dir vanished: ${E(r)??r})`),i("tengu_tool_cgroup",{status:S("vanished")});return}}
function Vo(e){return Ko(e,{throwIfNoEntry:!1})!==void 0}
function Mn(){let e=a.CLAUDE_CODE_TOOL_MEMORY_CGROUP_EXCLUDE?.trim(),n=Xo(e);if(n.has("mcp")||!Yo(!e))return n;return new Set([...n,"mcp"])}
function Xo(e){if(e)return Fn(e.split(","));let n=N.read("tengu_tool_cgroup_exclude_classes",null);if(Array.isArray(n))return Fn(n);return new Set(Xe)}
function Yo(e){let n=a.CLAUDE_CODE_MCP_MEMORY_CGROUP?.trim().toLowerCase();if(n&&(_s(n)||n==="none"))return!0;return e&&N.read("tengu_mcp_memory_cgroup",null)===!1}
function Fn(e){let n=new Set;for(let r of e){let o=typeof r==="string"?r.trim().toLowerCase():"";if(o==="all-new"){Xe.forEach((u)=>n.add(u));continue}let s=Xe.find((u)=>u===o);if(s!==void 0)n.add(s)}return n}
function Pl(e){let n=K(e);return n===void 0?{}:{cgroup:n}}
function ni(e){let n=/^oom_kill (\d+)$/m.exec(e);return n?Number.parseInt(n[1],10):void 0}
function Qe(e){return T.join(e.dir,e.v2?"memory.events":"memory.oom_control")}
function z(e){return G(e,"utf8")}
function Je(e,n=z){try{return ni(n(e))}catch{return}}
function Bn(e,n=z,r){try{let o=e.layout;if(!e.dir||o===void 0||o.reuse)return;let s=r??Je(Qe(o),n);if(s===void 0)return;let u=e.oomKillsSeen;if(e.oomKillsSeen=s,u===void 0||s<=u)return;t(`tool cgroup: OOM killer fired ${s-u} time(s) in ${o.dir} since last check`),i("tengu_tool_cgroup",{status:S("oom_kill"),oom_kills:s-u,cgroup_v2:o.v2})}catch(o){t(`tool cgroup: oom_kill check skipped (${o})`)}}
function zn(e,n,r={mkdirSync:Ye,writeFileSync:pe,rmdirSync:Nn}){if(e.reuse)return;let o=!0;try{r.mkdirSync(e.dir)}catch(s){if(E(s)!=="EEXIST")throw s;o=!1}try{r.writeFileSync(T.join(e.dir,e.v2?"memory.max":"memory.limit_in_bytes"),String(n))}catch(s){if(o)try{r.rmdirSync?.(e.dir)}catch{}throw s}}
function oi(e){let n=He(e);if(!n)return;return si(n.mountRoot,n.path,n.v2?T.dirname(n.path):n.path,n.v2)}
function si(e,n,r,o){let s=n.split("/").includes(Ve),u=T.join(e,n,".");return s?{dir:u,v2:o,reuse:!0,selfDir:u}:{dir:T.join(e,r,Ve),v2:o,reuse:!1,selfDir:u}}
function ai(e){return Math.floor(e-Math.max(2*Gn,e*0.15))}
function ui(e,n){if(e!==void 0)return e;let r=ai(n);return r>=qo?r:void 0}
function ci(e){let n=/^(\d+(?:\.\d+)?)\s*([kmgt]?)(?:i?b)?$/i.exec(e?.trim()??"");if(!n)return;let r=Math.floor(Number(n[1])*1024**" kmgt".indexOf((n[2]||" ").toLowerCase()));return r>=1?r:void 0}
function W({useToolMemoryCgroup:e=!0,toolCgroupClass:n="helper"}){return e?Pl(n):{}}
function et(){return!1}
async function Hg(e,n=[],r={}){$Ue();let{useToolMemoryCgroup:o,toolCgroupClass:s,...u}=r,c={...W({useToolMemoryCgroup:o,toolCgroupClass:s}),...u};if(et()){let l=YB(e);if(l===null)throw Error(`Command '${e}' not found or is in an unsafe location (current directory)`);return hut(l,n,c)}return hut(e,n,c)}
function nOs(e,n={}){$Ue();let{useToolMemoryCgroup:r,toolCgroupClass:o,...s}=n;return B(e,{...W({useToolMemoryCgroup:r,toolCgroupClass:o}),...s,shell:!0})}
export { $Ue as $Ue };
export { Pl as Pl };
export { tG as tG };
export { vHt as vHt };
export { hut as hut };
export { dY as dY };
export { nOs as nOs };
export { Hg as Hg };
