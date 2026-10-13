// Extracted source closure for 2.1.295; source module src_186578912.js sha256 8886b2923ae669a979e16ab15d7a8151571d85efcd7471e0ba226d4e2d53559f
import { kl, dr, sZ, X8, F0t, xPs, e9e, KCo, YCo, sWn, mUe, U0t, iWn, APs, Jb, zW, Onn, fUe, EPs, kPs, PPs, qCo, GCo, Xdt, pUe, $0t, Pnn, bPs, zTr, VTr, N0t, aWn, VCo, IPs, zCo, L$s, GTr, CPs, Jdt, O$s, H$s, M$s, P$s, I$s, RPs, D$s, vPs, TPs, wPs } from "./src_186567271.mjs";
var Ft=/^[cC][0-9a-z]{6,}$/;
var Ut=/^[0-9a-z]+$/;
var Mt=/^[0-7][0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{25}$/;
var Jt=/^[0-9a-vA-V]{20}$/;
var Bt=/^[A-Za-z0-9]{27}$/;
var Vt=/^[a-zA-Z0-9_-]{21}$/;
function Kt(e){return new RegExp(`^[a-zA-Z0-9_-]{${e}}$`)}
var Wt=/^P(?:(\d+W)|(?!.*W)(?=\d|T\d)(\d+Y)?(\d+M)?(\d+D)?(T(?=\d)(\d+H)?(\d+M)?(\d+([.,]\d+)?S)?)?)$/;
var Gt=/^([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})$/;
var ye=(e)=>{if(!e)return/^([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[1-8][0-9a-fA-F]{3}-[89abAB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}|00000000-0000-0000-0000-000000000000|ffffffff-ffff-ffff-ffff-ffffffffffff)$/;return new RegExp(`^([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-${e}[0-9a-fA-F]{3}-[89abAB][0-9a-fA-F]{3}-[0-9a-fA-F]{12})$`)};
var Yt=/^(?!\.)(?!.*\.\.)([A-Za-z0-9_'+\-\.]*)[A-Za-z0-9_+-]@([A-Za-z0-9][A-Za-z0-9\-]*\.)+[A-Za-z]{2,}$/;
var Fs="^[\\p{Extended_Pictographic}\\p{Emoji_Component}]+$";
function qt(){return new RegExp(Fs,"u")}
var Xt=/^(?:(?:25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9][0-9]|[0-9])\.){3}(?:25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9][0-9]|[0-9])$/;
var Ht=/^(([0-9a-fA-F]{1,4}:){7}[0-9a-fA-F]{1,4}|([0-9a-fA-F]{1,4}:){1,7}:|([0-9a-fA-F]{1,4}:){1,6}:[0-9a-fA-F]{1,4}|([0-9a-fA-F]{1,4}:){1,5}(:[0-9a-fA-F]{1,4}){1,2}|([0-9a-fA-F]{1,4}:){1,4}(:[0-9a-fA-F]{1,4}){1,3}|([0-9a-fA-F]{1,4}:){1,3}(:[0-9a-fA-F]{1,4}){1,4}|([0-9a-fA-F]{1,4}:){1,2}(:[0-9a-fA-F]{1,4}){1,5}|[0-9a-fA-F]{1,4}:((:[0-9a-fA-F]{1,4}){1,6})|:((:[0-9a-fA-F]{1,4}){1,7}|:))$/;
var Qt=/^((25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9][0-9]|[0-9])\.){3}(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9][0-9]|[0-9])\/([0-9]|[1-2][0-9]|3[0-2])$/;
var er=/^(([0-9a-fA-F]{1,4}:){7}[0-9a-fA-F]{1,4}|([0-9a-fA-F]{1,4}:){1,7}:|([0-9a-fA-F]{1,4}:){1,6}:[0-9a-fA-F]{1,4}|([0-9a-fA-F]{1,4}:){1,5}(:[0-9a-fA-F]{1,4}){1,2}|([0-9a-fA-F]{1,4}:){1,4}(:[0-9a-fA-F]{1,4}){1,3}|([0-9a-fA-F]{1,4}:){1,3}(:[0-9a-fA-F]{1,4}){1,4}|([0-9a-fA-F]{1,4}:){1,2}(:[0-9a-fA-F]{1,4}){1,5}|[0-9a-fA-F]{1,4}:((:[0-9a-fA-F]{1,4}){1,6})|:((:[0-9a-fA-F]{1,4}){1,7}|:))\/(12[0-8]|1[01][0-9]|[1-9]?[0-9])$/;
var tr=/^$|^(?:[0-9a-zA-Z+/]{4})*(?:(?:[0-9a-zA-Z+/]{2}==)|(?:[0-9a-zA-Z+/]{3}=))?$/;
var $e=/^[A-Za-z0-9_-]*$/;
var rr=/^https?$/;
var or=/^\+[1-9]\d{6,14}$/;
var nr="(?:(?:\\d\\d[2468][048]|\\d\\d[13579][26]|\\d\\d0[48]|[02468][048]00|[13579][26]00)-02-29|\\d{4}-(?:(?:0[13578]|1[02])-(?:0[1-9]|[12]\\d|3[01])|(?:0[469]|11)-(?:0[1-9]|[12]\\d|30)|(?:02)-(?:0[1-9]|1\\d|2[0-8])))";
function Us(e){return new RegExp(`^${e}$`)}
var sr=Us(nr);
function ze(e){return typeof e.precision==="number"?e.precision===-1?"(?:[01]\\d|2[0-3]):[0-5]\\d":e.precision===0?"(?:[01]\\d|2[0-3]):[0-5]\\d:[0-5]\\d":`(?:[01]\\d|2[0-3]):[0-5]\\d:[0-5]\\d\\.\\d{${e.precision}}`:e.seconds?"(?:[01]\\d|2[0-3]):[0-5]\\d:[0-5]\\d(?:\\.\\d+)?":"(?:[01]\\d|2[0-3]):[0-5]\\d(?::[0-5]\\d(?:\\.\\d+)?)?"}
function ir(e){return new RegExp(`^${ze(e)}$`)}
function cr(e){let t=["Z"];if(e.offset)t.push("([+-](?:[01]\\d|2[0-3]):[0-5]\\d)");let r=`${ze({precision:e.precision,seconds:!0})}(?:${t.join("|")})`,n=e.local?`${r}|${ze({precision:e.precision})}`:r;return new RegExp(`^${nr}T(?:${n})$`)}
var ur=(e)=>{let t=e?`[\\s\\S]{${e?.minimum??0},${e?.maximum??""}}`:"[\\s\\S]*";return new RegExp(`^${t}$`)};
var ee=/^-?\d+$/;
var D=/^-?\d+(?:\.\d+)?$/;
var ar=/^(?:true|false)$/i;
var lr=/^null$/i;
var pr=/^undefined$/i;
var fr=/^[^A-Z]*$/;
var mr=/^[^a-z]*$/;
var Z=dr("$ZodCheck",(e,t)=>{var r;e._zod??(e._zod={}),e._zod.def=t,(r=e._zod).onattach??(r.onattach=[])});
var be=(e)=>{let t=e.value;return!bPs(t)&&t.length!==void 0};
var te={number:"number",bigint:"bigint",object:"date"};
var ke=dr("$ZodCheckLessThan",(e,t)=>{Z.init(e,t);let r=te[typeof t.value];e._zod.onattach.push((n)=>{let s=n._zod.bag,i=(t.inclusive?s.maximum:s.exclusiveMaximum)??Number.POSITIVE_INFINITY;if(t.value<i)if(t.inclusive)s.maximum=t.value;else s.exclusiveMaximum=t.value}),e._zod.check=(n)=>{if(t.inclusive?n.value<=t.value:n.value<t.value)return;n.issues.push({origin:te[typeof n.value]??r,code:"too_big",maximum:typeof t.value==="object"?t.value.getTime():t.value,input:n.value,inclusive:t.inclusive,inst:e,continue:!t.abort})}});
var Ze=dr("$ZodCheckGreaterThan",(e,t)=>{Z.init(e,t);let r=te[typeof t.value];e._zod.onattach.push((n)=>{let s=n._zod.bag,i=(t.inclusive?s.minimum:s.exclusiveMinimum)??Number.NEGATIVE_INFINITY;if(t.value>i)if(t.inclusive)s.minimum=t.value;else s.exclusiveMinimum=t.value}),e._zod.check=(n)=>{if(t.inclusive?n.value>=t.value:n.value>t.value)return;n.issues.push({origin:te[typeof n.value]??r,code:"too_small",minimum:typeof t.value==="object"?t.value.getTime():t.value,input:n.value,inclusive:t.inclusive,inst:e,continue:!t.abort})}});
var _r=dr("$ZodCheckMultipleOf",(e,t)=>{Z.init(e,t),e._zod.onattach.push((r)=>{var n;(n=r._zod.bag).multipleOf??(n.multipleOf=t.value)}),e._zod.check=(r)=>{if(typeof r.value!==typeof t.value)throw Error("Cannot mix number and bigint in multiple_of check.");if(typeof r.value==="bigint"?t.value!==BigInt(0)&&r.value%t.value===BigInt(0):wPs(r.value,t.value)===0)return;r.issues.push({origin:typeof r.value,code:"not_multiple_of",divisor:t.value,input:r.value,inst:e,continue:!t.abort})}});
var gr=dr("$ZodCheckNumberFormat",(e,t)=>{Z.init(e,t),t.format=t.format||"float64";let r=t.format?.includes("int"),n=r?"int":"number",[s,i]=TPs[t.format];e._zod.onattach.push((c)=>{let a=c._zod.bag;if(a.format=t.format,a.minimum=s,a.maximum=i,r)a.pattern=ee}),e._zod.check=(c)=>{let a=c.value;if(r){if(!Number.isInteger(a)){c.issues.push({expected:n,format:t.format,code:"invalid_type",continue:!1,input:a,inst:e});return}if(!Number.isSafeInteger(a)){if(a>0)c.issues.push({input:a,code:"too_big",maximum:Number.MAX_SAFE_INTEGER,note:"Integers must be within the safe integer range.",inst:e,origin:n,inclusive:!0,continue:!t.abort});else c.issues.push({input:a,code:"too_small",minimum:Number.MIN_SAFE_INTEGER,note:"Integers must be within the safe integer range.",inst:e,origin:n,inclusive:!0,continue:!t.abort});return}}if(a<s)c.issues.push({origin:"number",input:a,code:"too_small",minimum:s,inclusive:!0,inst:e,continue:!t.abort});if(a>i)c.issues.push({origin:"number",input:a,code:"too_big",maximum:i,inclusive:!0,inst:e,continue:!t.abort})}});
var xr=dr("$ZodCheckMaxLength",(e,t)=>{var r;Z.init(e,t),(r=e._zod.def).when??(r.when=be),e._zod.onattach.push((n)=>{let s=n._zod.bag.maximum??Number.POSITIVE_INFINITY;if(t.maximum<s)n._zod.bag.maximum=t.maximum}),e._zod.check=(n)=>{let s=n.value,i=s.length;if((typeof s==="string"&&i>t.maximum?zTr(s):i)<=t.maximum)return;let a=VTr(s);n.issues.push({origin:a,code:"too_big",maximum:t.maximum,inclusive:!0,input:s,inst:e,continue:!t.abort})}});
var vr=dr("$ZodCheckMinLength",(e,t)=>{var r;Z.init(e,t),(r=e._zod.def).when??(r.when=be),e._zod.onattach.push((n)=>{let s=n._zod.bag.minimum??Number.NEGATIVE_INFINITY;if(t.minimum>s)n._zod.bag.minimum=t.minimum}),e._zod.check=(n)=>{let s=n.value,i=s.length;if((typeof s==="string"&&i>=t.minimum&&i<t.minimum*2?zTr(s):i)>=t.minimum)return;let a=VTr(s);n.issues.push({origin:a,code:"too_small",minimum:t.minimum,inclusive:!0,input:s,inst:e,continue:!t.abort})}});
var zr=dr("$ZodCheckLengthEquals",(e,t)=>{var r;Z.init(e,t),(r=e._zod.def).when??(r.when=be),e._zod.onattach.push((n)=>{let s=n._zod.bag;s.minimum=t.length,s.maximum=t.length,s.length=t.length}),e._zod.check=(n)=>{let s=n.value,i=s.length,c=typeof s==="string"&&i>=t.length&&i<=t.length*2?zTr(s):i;if(c===t.length)return;let a=VTr(s),l=c>t.length;n.issues.push({origin:a,...l?{code:"too_big",maximum:t.length}:{code:"too_small",minimum:t.length},inclusive:!0,exact:!0,input:n.value,inst:e,continue:!t.abort})}});
var L=dr("$ZodCheckStringFormat",(e,t)=>{var r,n;if(Z.init(e,t),e._zod.onattach.push((s)=>{let i=s._zod.bag;if(i.format=t.format,t.pattern)i.patterns??(i.patterns=new Set),i.patterns.add(t.pattern)}),t.pattern)(r=e._zod).check??(r.check=(s)=>{if(t.pattern.lastIndex=0,t.pattern.test(s.value))return;s.issues.push({origin:"string",code:"invalid_format",format:t.format,input:s.value,...t.pattern?{pattern:t.pattern.toString()}:{},inst:e,continue:!t.abort})});else(n=e._zod).check??(n.check=()=>{})});
var yr=dr("$ZodCheckRegex",(e,t)=>{L.init(e,t),e._zod.check=(r)=>{if(t.pattern.lastIndex=0,t.pattern.test(r.value))return;r.issues.push({origin:"string",code:"invalid_format",format:"regex",input:r.value,pattern:t.pattern.toString(),inst:e,continue:!t.abort})}});
var $r=dr("$ZodCheckLowerCase",(e,t)=>{t.pattern??(t.pattern=fr),L.init(e,t)});
var br=dr("$ZodCheckUpperCase",(e,t)=>{t.pattern??(t.pattern=mr),L.init(e,t)});
var kr=dr("$ZodCheckIncludes",(e,t)=>{Z.init(e,t);let r=Jdt(t.includes),n=new RegExp(typeof t.position==="number"?`^.{${t.position},}${r}`:r);t.pattern=n,e._zod.onattach.push((s)=>{let i=s._zod.bag;i.patterns??(i.patterns=new Set),i.patterns.add(n)}),e._zod.check=(s)=>{if(s.value.includes(t.includes,t.position))return;s.issues.push({origin:"string",code:"invalid_format",format:"includes",includes:t.includes,input:s.value,inst:e,continue:!t.abort})}});
var Zr=dr("$ZodCheckStartsWith",(e,t)=>{Z.init(e,t);let r=new RegExp(`^${Jdt(t.prefix)}.*`);t.pattern??(t.pattern=r),e._zod.onattach.push((n)=>{let s=n._zod.bag;s.patterns??(s.patterns=new Set),s.patterns.add(r)}),e._zod.check=(n)=>{if(n.value.startsWith(t.prefix))return;n.issues.push({origin:"string",code:"invalid_format",format:"starts_with",prefix:t.prefix,input:n.value,inst:e,continue:!t.abort})}});
var wr=dr("$ZodCheckEndsWith",(e,t)=>{Z.init(e,t);let r=new RegExp(`.*${Jdt(t.suffix)}$`);t.pattern??(t.pattern=r),e._zod.onattach.push((n)=>{let s=n._zod.bag;s.patterns??(s.patterns=new Set),s.patterns.add(r)}),e._zod.check=(n)=>{if(n.value.endsWith(t.suffix))return;n.issues.push({origin:"string",code:"invalid_format",format:"ends_with",suffix:t.suffix,input:n.value,inst:e,continue:!t.abort})}});
var Pr=dr("$ZodCheckOverwrite",(e,t)=>{Z.init(e,t),e._zod.check=(r)=>{r.value=t.tx(r.value)}});
var Q5e=globalThis.__zod_globalRegistry;
class we{constructor(e=[],t={}){this.content=[],this.indent=0,this.args=e,this.closed=t}indented(e){this.indent+=1,e(this),this.indent-=1}write(e){if(typeof e==="function"){e(this,{execution:"sync"}),e(this,{execution:"async"});return}let r=e.split(`
`).filter((i)=>i),n=Math.min(...r.map((i)=>i.length-i.trimStart().length)),s=r.map((i)=>i.slice(n)).map((i)=>" ".repeat(this.indent*2)+i);for(let i of s)this.content.push(i)}compile(){let e=Function,t=this?.content??[""];return new e(...Object.keys(this.closed),`return function (${this.args.join(", ")}) {
${t.join(`
`)}
};`)(...Object.values(this.closed))}}
function Js(){let e=this._zod;return e.message??(e.message=JSON.stringify(e.def,sWn,2)),e.message}
function Bs(e){this._zod.message=e}
var Vs={get:Js,set:Bs,enumerable:!0,configurable:!0};
var Pe={value:void 0,enumerable:!1};
var Se={value:void 0,enumerable:!1};
var Tr=new WeakSet([Object.prototype,Error.prototype]);
var Ar=(e,t)=>{e.name="$ZodError",Pe.value=e._zod,Object.defineProperty(e,"_zod",Pe),Se.value=t,Object.defineProperty(e,"issues",Se),Pe.value=void 0,Se.value=void 0,Object.defineProperty(e,"message",Vs);let r=Object.getPrototypeOf(e);if(!Tr.has(r))Tr.add(r),Object.defineProperty(r,"toString",{configurable:!0,enumerable:!1,get(){let n=()=>this.message;return Object.defineProperty(this,"toString",{value:n,configurable:!0,writable:!0}),n},set(n){Object.defineProperty(this,"toString",{value:n,configurable:!0,writable:!0})}})};
var re=dr("$ZodError",Ar);
var Ie=dr("$ZodError",Ar,void 0,{Parent:Error});
function Ks(e,t,r){if(!Object.prototype.hasOwnProperty.call(e,t))if(t==="__proto__")Object.defineProperty(e,t,{value:r(),writable:!0,enumerable:!0,configurable:!0});else e[t]=r();return e[t]}
function Er(e,t=(r)=>r.message){let r={},n=[];for(let s of e.issues)if(s.path.length>0)Ks(r,s.path[0],()=>[]).push(t(s));else n.push(t(s));return{formErrors:n,fieldErrors:r}}
function Nr(e,t=(r)=>r.message){let r={_errors:[]},n=(s,i=[])=>{for(let c of s.issues)if(c.code==="invalid_union"&&c.errors.length)c.errors.map((a)=>n({issues:a},[...i,...c.path]));else if(c.code==="invalid_key")n({issues:c.issues},[...i,...c.path]);else if(c.code==="invalid_element")n({issues:c.issues},[...i,...c.path]);else{let a=[...i,...c.path];if(a.length===0)r._errors.push(t(c));else{let l=r,p=0;while(p<a.length){let f=a[p],d=p===a.length-1;if(f==="_errors"){if(d)l._errors.push(t(c));p++;continue}if(!Object.prototype.hasOwnProperty.call(l,f))Object.defineProperty(l,f,{value:{_errors:[]},enumerable:!0,writable:!0,configurable:!0});let h=l[f];if(d)h._errors.push(t(c));l=h,p++}}}};return n(e),r}
function ne(e,t){return{callee:t?.callee??e,Err:t?.Err}}
var se=(e)=>{let t=(r,n,s,i)=>{let c=s?{...s,async:!1}:{async:!1},a=r._zod.run({value:n,issues:[]},c);if(a instanceof Promise)throw new e9e;if(a.issues.length){let l=new(i?.Err??e)(a.issues.map((p)=>mUe(p,c,X8())));throw zCo(l,i?.callee??t),l}return a.value};return t};
var ie=(e)=>{let t=async(r,n,s,i)=>{let c=s?{...s,async:!0}:{async:!0},a=r._zod.run({value:n,issues:[]},c);if(a instanceof Promise)a=await a;if(a.issues.length){let l=new(i?.Err??e)(a.issues.map((p)=>mUe(p,c,X8())));throw zCo(l,i?.callee??t),l}return a.value};return t};
var F=(e)=>(t,r,n)=>{let s=n?{...n,async:!1}:{async:!1},i=t._zod.run({value:r,issues:[]},s);if(i instanceof Promise)throw new e9e;return i.issues.length?{success:!1,error:new(e??re)(i.issues.map((c)=>mUe(c,s,X8())))}:{success:!0,data:i.value}};
var eWn=F(Ie);
var M=(e)=>async(t,r,n)=>{let s=n?{...n,async:!0}:{async:!0},i=t._zod.run({value:r,issues:[]},s);if(i instanceof Promise)i=await i;return i.issues.length?{success:!1,error:new e(i.issues.map((c)=>mUe(c,s,X8())))}:{success:!0,data:i.value}};
var Cr=M(Ie);
var Rr=(e)=>{let t=se(e),r=(n,s,i,c)=>{let a=i?{...i,direction:"backward"}:{direction:"backward"};return t(n,s,a,ne(r,c))};return r};
var Dr=(e)=>{let t=se(e),r=(n,s,i,c)=>t(n,s,i,ne(r,c));return r};
var jr=(e)=>{let t=ie(e),r=async(n,s,i,c)=>{let a=i?{...i,direction:"backward"}:{direction:"backward"};return await t(n,s,a,ne(r,c))};return r};
var Lr=(e)=>{let t=ie(e),r=async(n,s,i,c)=>await t(n,s,i,ne(r,c));return r};
var Fr=(e)=>(t,r,n)=>{let s=n?{...n,direction:"backward"}:{direction:"backward"};return F(e)(t,r,s)};
var Ur=(e)=>(t,r,n)=>F(e)(t,r,n);
var Mr=(e)=>async(t,r,n)=>{let s=n?{...n,direction:"backward"}:{direction:"backward"};return M(e)(t,r,s)};
var Jr=(e)=>async(t,r,n)=>M(e)(t,r,n);
var Vr={major:4,minor:5,patch:4};
var ME=dr("$ZodType",(e,t)=>{var r;e??(e={}),e._zod.def=t,e._zod.bag=e._zod.bag||{},e._zod.version=Vr;let n=e._zod.def.checks,s=e._zod.traits.has("$ZodCheck")?[e,...n??[]]:n?.length?[...n]:[];for(let i of s)for(let c of i._zod.onattach)c(e);if(s.length===0)(r=e._zod).deferred??(r.deferred=[]),e._zod.deferred?.push(()=>{e._zod.run=e._zod.parse});else{let i=(a,l,p)=>{if(a.memo)return a;let f=F0t(a),d;for(let h of l){if(h._zod.def.when){if(xPs(a))continue;if(!h._zod.def.when(a))continue}else if(f)continue;let m=a.issues.length,_=h._zod.check(a);if(_ instanceof Promise&&p?.async===!1)throw new e9e;if(d||_ instanceof Promise)d=(d??Promise.resolve()).then(async()=>{if(await _,a.issues.length===m)return;if(KCo(a.issues,m,e),!f)f=F0t(a,m)});else{if(a.issues.length===m)continue;if(KCo(a.issues,m,e),!f)f=F0t(a,m)}}if(d)return d.then(()=>a);return a},c=(a,l,p)=>{if(F0t(a))return a.aborted=!0,a;let f=i(l,s,p);if(f instanceof Promise){if(p.async===!1)throw new e9e;return f.then((d)=>e._zod.parse(d,p))}return e._zod.parse(f,p)};e._zod.run=(a,l)=>{if(l.skipChecks)return e._zod.parse(a,l);if(l.direction==="backward"){let f=e._zod.parse({value:a.value,issues:[]},{...l,skipChecks:!0});if(f instanceof Promise)return f.then((d)=>c(d,a,l));return c(f,a,l)}let p=e._zod.parse(a,l);if(p instanceof Promise){if(l.async===!1)throw new e9e;return p.then((f)=>i(f,s,l))}return i(p,s,l)}}},{get "~standard"(){return YCo(this,"~standard",Ae(this))},set "~standard"(e){U0t(this,"~standard",e)}});
var Kr=(e)=>e.success?{value:e.data}:{issues:e.error?.issues};
function Ae(e){return{validate:(t)=>{try{return Kr(eWn(e,t))}catch(r){return Cr(e,t).then(Kr)}},vendor:"zod",version:1}}
var ae=dr("$ZodString",(e,t)=>{ME.init(e,t),e._zod.pattern=[...e?._zod.bag?.patterns??[]].pop()??ur(e._zod.bag),e._zod.parse=(r,n)=>{if(t.coerce)try{r.value=String(r.value)}catch(s){}if(typeof r.value==="string")return r;return r.issues.push({expected:"string",code:"invalid_type",input:r.value,inst:e}),r}});
var z=dr("$ZodStringFormat",(e,t)=>{L.init(e,t),ae.init(e,t)});
var so=dr("$ZodGUID",(e,t)=>{t.pattern??(t.pattern=Gt),z.init(e,t)});
var io=dr("$ZodUUID",(e,t)=>{if(t.version){let n={v1:1,v2:2,v3:3,v4:4,v5:5,v6:6,v7:7,v8:8}[t.version];if(n===void 0)throw Error(`Invalid UUID version: "${t.version}"`);t.pattern??(t.pattern=ye(n))}else t.pattern??(t.pattern=ye());z.init(e,t)});
var co=dr("$ZodEmail",(e,t)=>{t.pattern??(t.pattern=Yt),z.init(e,t)});
var uo=1;
var ao=2;
function Gs(e,t){if(!t.normalize&&t.protocol?.source===rr.source&&!/^https?:\/\//i.test(e))return uo;try{return new URL(e)}catch{return ao}}
var Ys=/[\t\n\r]/g;
function qs(e){return e.replace(Ys,"")}
function Xs(e,t){return t.lastIndex=0,t.test(e.hostname)}
function Hs(e,t){return t.lastIndex=0,t.test(e.protocol.endsWith(":")?e.protocol.slice(0,-1):e.protocol)}
var lo=dr("$ZodURL",(e,t)=>{z.init(e,t),e._zod.check=(r)=>{try{let n=r.value.trim(),s=Gs(n,t);if(s===uo){r.issues.push({code:"invalid_format",format:"url",note:"Invalid URL format",input:r.value,inst:e,continue:!t.abort});return}if(s===ao){r.issues.push({code:"invalid_format",format:"url",input:r.value,inst:e,continue:!t.abort});return}if(t.hostname&&!Xs(s,t.hostname))r.issues.push({code:"invalid_format",format:"url",note:"Invalid hostname",pattern:t.hostname.source,input:r.value,inst:e,continue:!t.abort});if(t.protocol&&!Hs(s,t.protocol))r.issues.push({code:"invalid_format",format:"url",note:"Invalid protocol",pattern:t.protocol.source,input:r.value,inst:e,continue:!t.abort});r.value=t.normalize?s.href:qs(n);return}catch(n){r.issues.push({code:"invalid_format",format:"url",input:r.value,inst:e,continue:!t.abort})}}});
var po=dr("$ZodEmoji",(e,t)=>{t.pattern??(t.pattern=qt()),z.init(e,t)});
var fo=dr("$ZodNanoID",(e,t)=>{if(t.length!==void 0&&(!Number.isInteger(t.length)||t.length<1))throw Error(`Invalid nanoid length: ${t.length}`);t.pattern??(t.pattern=t.length===void 0?Vt:Kt(t.length)),z.init(e,t)});
var mo=dr("$ZodCUID",(e,t)=>{t.pattern??(t.pattern=Ft),z.init(e,t)});
var ho=dr("$ZodCUID2",(e,t)=>{t.pattern??(t.pattern=Ut),z.init(e,t)});
var _o=dr("$ZodULID",(e,t)=>{t.pattern??(t.pattern=Mt),z.init(e,t)});
var go=dr("$ZodXID",(e,t)=>{t.pattern??(t.pattern=Jt),z.init(e,t)});
var xo=dr("$ZodKSUID",(e,t)=>{t.pattern??(t.pattern=Bt),z.init(e,t)});
var vo=dr("$ZodISODateTime",(e,t)=>{if(t.pattern??(t.pattern=cr(t)),z.init(e,t),t.local||t.precision===-1)e._zod.bag.laxFormat=!0,e._zod.onattach.push((r)=>{r._zod.bag.laxFormat=!0})});
var zo=dr("$ZodISODate",(e,t)=>{t.pattern??(t.pattern=sr),z.init(e,t)});
var yo=dr("$ZodISOTime",(e,t)=>{t.pattern??(t.pattern=ir(t)),z.init(e,t)});
var $o=dr("$ZodISODuration",(e,t)=>{t.pattern??(t.pattern=Wt),z.init(e,t)});
var bo=dr("$ZodIPv4",(e,t)=>{t.pattern??(t.pattern=Xt),z.init(e,t),e._zod.bag.format="ipv4"});
var Qs=/^[0-9a-fA-F:.]+$/;
function ko(e){if(!Qs.test(e))return!1;try{return new URL(`http://[${e}]`),!0}catch{return!1}}
var Zo=dr("$ZodIPv6",(e,t)=>{t.pattern??(t.pattern=Ht),z.init(e,t),e._zod.bag.format="ipv6",e._zod.check=(r)=>{if(!ko(r.value))r.issues.push({code:"invalid_format",format:"ipv6",input:r.value,inst:e,continue:!t.abort})}});
var wo=dr("$ZodCIDRv4",(e,t)=>{t.pattern??(t.pattern=Qt),z.init(e,t)});
function ei(e){let t=e.split("/");if(t.length!==2)return!1;let[r,n]=t;if(!n)return!1;let s=Number(n);if(`${s}`!==n)return!1;if(s<0||s>128)return!1;return ko(r)}
var Po=dr("$ZodCIDRv6",(e,t)=>{t.pattern??(t.pattern=er),z.init(e,t),e._zod.check=(r)=>{if(!ei(r.value))r.issues.push({code:"invalid_format",format:"cidrv6",input:r.value,inst:e,continue:!t.abort})}});
function So(e){if(e==="")return!0;if(/\s/.test(e))return!1;if(e.length%4!==0)return!1;try{return atob(e),!0}catch{return!1}}
var Io=dr("$ZodBase64",(e,t)=>{t.pattern??(t.pattern=tr),z.init(e,t),e._zod.bag.contentEncoding="base64",e._zod.check=(r)=>{if(So(r.value))return;r.issues.push({code:"invalid_format",format:"base64",input:r.value,inst:e,continue:!t.abort})}});
function ti(e){if(!$e.test(e))return!1;let t=e.replace(/[-_]/g,(n)=>n==="-"?"+":"/"),r=t.padEnd(Math.ceil(t.length/4)*4,"=");return So(r)}
var Oo=dr("$ZodBase64URL",(e,t)=>{t.pattern??(t.pattern=$e),z.init(e,t),e._zod.bag.contentEncoding="base64url",e._zod.check=(r)=>{if(ti(r.value))return;r.issues.push({code:"invalid_format",format:"base64url",input:r.value,inst:e,continue:!t.abort})}});
var To=dr("$ZodE164",(e,t)=>{t.pattern??(t.pattern=or),z.init(e,t)});
function ri(e,t=null){try{let r=e.split(".");if(r.length!==3)return!1;let[n]=r;if(!n)return!1;let s=JSON.parse(atob(n));if("typ"in s&&s?.typ!=="JWT")return!1;if(!s.alg)return!1;if(t&&(!("alg"in s)||s.alg!==t))return!1;return!0}catch{return!1}}
var Ao=dr("$ZodJWT",(e,t)=>{z.init(e,t),e._zod.check=(r)=>{if(ri(r.value,t.alg))return;r.issues.push({code:"invalid_format",format:"jwt",input:r.value,inst:e,continue:!t.abort})}});
var Ee=dr("$ZodNumber",(e,t)=>{ME.init(e,t),e._zod.pattern=e._zod.bag.pattern??D,e._zod.parse=(r,n)=>{if(t.coerce)try{r.value=Number(r.value)}catch(c){}let s=r.value;if(typeof s==="number"&&!Number.isNaN(s)&&Number.isFinite(s))return r;let i=typeof s==="number"?Number.isNaN(s)?"NaN":!Number.isFinite(s)?String(s):void 0:void 0;return r.issues.push({expected:"number",code:"invalid_type",input:s,inst:e,...i?{received:i}:{}}),r}});
var Eo=dr("$ZodNumberFormat",(e,t)=>{gr.init(e,t),Ee.init(e,t)});
var No=dr("$ZodBoolean",(e,t)=>{ME.init(e,t),e._zod.pattern=ar,e._zod.parse=(r,n)=>{if(t.coerce)try{r.value=Boolean(r.value)}catch(i){}let s=r.value;if(typeof s==="boolean")return r;return r.issues.push({expected:"boolean",code:"invalid_type",input:s,inst:e}),r}});
var Co=dr("$ZodUndefined",(e,t)=>{ME.init(e,t),e._zod.pattern=pr,e._zod.values=new Set([void 0]),e._zod.parse=(r,n)=>{let s=r.value;if(typeof s>"u")return r;return r.issues.push({expected:"undefined",code:"invalid_type",input:s,inst:e}),r}});
var Ro=dr("$ZodNull",(e,t)=>{ME.init(e,t),e._zod.pattern=lr,e._zod.values=new Set([null]),e._zod.parse=(r,n)=>{let s=r.value;if(s===null)return r;return r.issues.push({expected:"null",code:"invalid_type",input:s,inst:e}),r}});
var jo=dr("$ZodUnknown",(e,t)=>{ME.init(e,t),e._zod.parse=(r)=>r});
var Lo=dr("$ZodNever",(e,t)=>{ME.init(e,t),e._zod.parse=(r,n)=>(r.issues.push({expected:"never",code:"invalid_type",input:r.value,inst:e}),r)});
function Wr(e,t,r){if(e.issues.length)t.issues.push(...fUe(r,e.issues));t.value[r]=e.value}
var Fo=dr("$ZodArray",(e,t)=>{ME.init(e,t);let r=sZ.memoizer;r?.attach(e),e._zod.parse=(n,s)=>{let i=n.value;if(!Array.isArray(i))return n.issues.push({expected:"array",code:"invalid_type",input:i,inst:e}),n;n.value=r?r.alloc(e,n,Array(i.length),s):Array(i.length);let c=[];for(let a=0;a<i.length;a++){let l=i[a],p=t.element._zod.run({value:l,issues:[]},s);if(p instanceof Promise)c.push(p.then((f)=>Wr(f,n,a)));else Wr(p,n,a)}if(c.length)return Promise.all(c).then(()=>n);return n}});
function ue(e,t,r,n,s,i){let c=r in n,a=i==="optional";if(!c&&a&&s==="optional")return;if(e.issues.length){if(s!==void 0&&a&&!c)return;t.issues.push(...fUe(r,e.issues))}if(!c&&s===void 0){if(!e.issues.length)t.issues.push({code:"invalid_type",expected:"nonoptional",input:void 0,path:[r]});return}if(e.value===void 0){if(c)t.value[r]=void 0}else t.value[r]=e.value}
var oi=[];
function Uo(e){let t=Object.keys(e.shape),r=Object.getOwnPropertySymbols(e.shape),n=r.length?r:oi,s=n.length?[...t,...n]:t;for(let c of s)if(!e.shape?.[c]?._zod?.traits?.has("$ZodType"))throw Error(`Invalid element at key "${String(c)}": expected a Zod schema`);let i=APs(e.shape);return{...e,allKeys:s,symbolKeys:n,keySet:new Set(t),numKeys:t.length,optionalKeys:new Set(i)}}
function Mo(e,t,r,n,s,i){let c=[],a=s.keySet,l=s.catchall._zod,p=l.def.type,{optin:f,optout:d}=l;for(let h in t){if(a.has(h))continue;if(h==="__proto__"){if(p==="never")c.push(h);continue}if(p==="never"){c.push(h);continue}let m=l.run({value:t[h],issues:[]},n);if(m instanceof Promise)e.push(m.then((_)=>ue(_,r,h,t,f,d)));else ue(m,r,h,t,f,d)}if(c.length)r.issues.push({code:"unrecognized_keys",keys:c,input:t,inst:i,continue:!0});if(!e.length)return r;return Promise.all(e).then(()=>r)}
var Oe=new WeakMap;
var ni=dr("$ZodObject",(e,t)=>{if(ME.init(e,t),!Object.getOwnPropertyDescriptor(t,"shape")?.get){let l=t.shape;Oe.set(t,l),Object.defineProperty(t,"shape",{get:()=>{let p={...l};return Object.defineProperty(t,"shape",{value:p}),Oe.set(t,p),p}})}let n=iWn(()=>Uo(t));Jb(e,"propValues",(l)=>{let p=l.def.shape,f={};for(let d in p){let h=p[d]._zod;if(h.values){if(!Object.prototype.hasOwnProperty.call(f,d))zW(f,d,new Set);for(let m of h.values)f[d].add(m);if(h.optin!==void 0)f[d].add(void 0)}}return f});let s=Onn,i=t.catchall,c,a=sZ.memoizer;a?.attach(e),e._zod.parse=(l,p)=>{c??(c=n.value);let f=l.value;if(!s(f))return l.issues.push({expected:"object",code:"invalid_type",input:f,inst:e}),l;l.value=a?a.alloc(e,l,{},p):{};let d=[],h=c.shape;for(let m of c.allKeys){if(m==="__proto__")continue;let _=h[m],x=_._zod.optin,g=_._zod.optout,O=_._zod.run({value:f[m],issues:[]},p);if(O instanceof Promise)d.push(O.then((N)=>ue(N,l,m,f,x,g)));else ue(O,l,m,f,x,g)}if(!i)return d.length?Promise.all(d).then(()=>l):l;return Mo(d,f,l,p,n.value,e)}});
var Jo=dr("$ZodObjectJIT",(e,t)=>{ni.init(e,t);let r=e._zod.parse,n=iWn(()=>Uo(t)),s=sZ.memoizer,i=(m)=>{let _=n.value,x=_.symbolKeys,g=new we(["payload","ctx"],{shape:m,inst:e,memo:s,syms:x}),O=(P)=>`shape[${P}]._zod.run({ value: input[${P}], issues: [] }, ctx)`,N=(P,y)=>`
          for (let i = 0; i < ${P}.issues.length; i++) {
            const iss = ${P}.issues[i];
            iss.path = iss.path ? [${y}, ...iss.path] : [${y}];
            payload.issues.push(iss);
          }`;g.write("const input = payload.value;");let Rt=Object.create(null),Ds=0;for(let P of _.allKeys)Rt[P]=`key_${Ds++}`;g.write(s?"const newResult = memo.alloc(inst, payload, {}, ctx);":"const newResult = {};");for(let P of _.allKeys){if(P==="__proto__")continue;let y=Rt[P],I=typeof P==="symbol"?`syms[${x.indexOf(P)}]`:EPs(P),ve=`${I} in input`,Dt=m[P],jt=Dt?._zod?.optin,Lt=jt!==void 0,js=Dt?._zod?.optout==="optional";if(g.write(`const ${y} = ${O(I)};`),Lt&&js){let Ls=jt==="optional"?`${y}_present`:`${y}.value !== undefined || ${y}_present`;g.write(`
        const ${y}_present = ${ve};
        if (!${y}.issues.length || ${y}_present) {
          if (${y}.issues.length) {${N(y,I)}
          }

          if (${Ls}) {
            newResult[${I}] = ${y}.value;
          }
        }

      `)}else if(!Lt)g.write(`
        const ${y}_present = ${ve};
        if (${y}.issues.length) {${N(y,I)}
        }
        if (!${y}_present && !${y}.issues.length) {
          payload.issues.push({
            code: "invalid_type",
            expected: "nonoptional",
            input: undefined,
            path: [${I}]
          });
        }

        if (${y}_present) {
          newResult[${I}] = ${y}.value;
        }

      `);else g.write(`
        if (${y}.issues.length) {${N(y,I)}
        }
        
        if (${y}.value === undefined) {
          if (${ve}) {
            newResult[${I}] = undefined;
          }
        } else {
          newResult[${I}] = ${y}.value;
        }

      `)}return g.write("payload.value = newResult;"),g.write("return payload;"),g.compile()},c,a=Onn,l=!sZ.jitless,f=l&&kPs.value,d=t.catchall,h;e._zod.parse=(m,_)=>{h??(h=n.value);let x=m.value;if(!a(x))return m.issues.push({expected:"object",code:"invalid_type",input:x,inst:e}),m;if(l&&f&&_?.async===!1&&_.jitless!==!0){if(!c)c=i(t.shape);if(m=c(m,_),!d)return m;return Mo([],x,m,_,h,e)}return r(m,_)}});
function Gr(e,t,r,n){for(let i of e)if(i.issues.length===0)return t.value=i.value,t;let s=e.filter((i)=>!F0t(i));if(s.length===1)return t.value=s[0].value,s[0];return t.issues.push({code:"invalid_union",input:t.value,inst:r,errors:e.map((i)=>i.issues.map((c)=>mUe(c,n,X8())))}),t}
var Ce=dr("$ZodUnion",(e,t)=>{ME.init(e,t),Jb(e,"optin",(n)=>n.def.options.some((s)=>s._zod.optin==="defaulted")?"defaulted":n.def.options.some((s)=>s._zod.optin!==void 0)?"optional":void 0),Jb(e,"optout",(n)=>n.def.options.some((s)=>s._zod.optout==="optional")?"optional":void 0),Jb(e,"values",(n)=>{if(n.def.options.every((s)=>s._zod.values))return new Set(n.def.options.flatMap((s)=>Array.from(s._zod.values)));return}),Jb(e,"pattern",(n)=>{if(n.def.options.every((s)=>s._zod.pattern)){let s=n.def.options.map((i)=>i._zod.pattern);return new RegExp(`^(${s.map((i)=>Pnn(i.source)).join("|")})$`)}return});let r=t.options.length===1?t.options[0]._zod.run:null;e._zod.parse=(n,s)=>{if(r)return r(n,s);let i=!1,c=[];for(let a of t.options){let l=a._zod.run({value:n.value,issues:[]},s);if(l instanceof Promise)c.push(l),i=!0;else{if(l.issues.length===0)return l;c.push(l)}}if(!i)return Gr(c,n,e,s);return Promise.all(c).then((a)=>Gr(a,n,e,s))}});
var Bo=dr("$ZodDiscriminatedUnion",(e,t)=>{t.inclusive=!1,Ce.init(e,t);let r=e._zod.parse;Jb(e,"propValues",(s)=>{let i={};for(let c of s.def.options){let a=c._zod.propValues;if(!a||Object.keys(a).length===0)throw Error(`Invalid discriminated union option at index "${s.def.options.indexOf(c)}"`);for(let[l,p]of Object.entries(a)){if(!Object.prototype.hasOwnProperty.call(i,l))zW(i,l,new Set);for(let f of p)i[l].add(f)}}return i}),t.options.forEach((s,i)=>{let c=Oe.get(s._zod.def);if(c&&!Object.prototype.hasOwnProperty.call(c,t.discriminator))throw Error(`Invalid discriminated union option at index "${i}"`)});let n=iWn(()=>{let s=t.options,i=new Map;for(let c of s){let a=c._zod.propValues?.[t.discriminator];if(!a||a.size===0)throw Error(`Invalid discriminated union option at index "${t.options.indexOf(c)}"`);for(let l of a){if(i.has(l))throw Error(`Duplicate discriminator value "${String(l)}"`);i.set(l,c)}}return i});e._zod.parse=(s,i)=>{let c=s.value;if(!Onn(c))return s.issues.push({code:"invalid_type",expected:"object",input:c,inst:e}),s;let a=n.value.get(c?.[t.discriminator]);if(a)return a._zod.run(s,i);if(t.unionFallback||i.direction==="backward")return r(s,i);return s.issues.push({code:"invalid_union",errors:[],note:"No matching discriminator",discriminator:t.discriminator,options:Array.from(n.value.keys()),input:c,path:[t.discriminator],inst:e}),s}});
var Vo=dr("$ZodIntersection",(e,t)=>{ME.init(e,t),e._zod.parse=(r,n)=>{let s=r.value,i=t.left._zod.run({value:s,issues:[]},n),c=t.right._zod.run({value:s,issues:[]},n);if(i instanceof Promise||c instanceof Promise)return Promise.all([i,c]).then(([l,p])=>Yr(r,l,p));return Yr(r,i,c)}});
function Te(e,t){if(e===t)return{valid:!0,data:e};if(e instanceof Date&&t instanceof Date&&+e===+t)return{valid:!0,data:e};if(N0t(e)&&N0t(t)){let r=Object.keys(t),n=Object.keys(e).filter((i)=>r.indexOf(i)!==-1),s={...e,...t};if(Object.prototype.hasOwnProperty.call(s,"__proto__"))delete s.__proto__;for(let i of n){if(i==="__proto__")continue;let c=Te(e[i],t[i]);if(!c.valid)return{valid:!1,mergeErrorPath:[i,...c.mergeErrorPath]};s[i]=c.data}return{valid:!0,data:s}}if(Array.isArray(e)&&Array.isArray(t)){if(e.length!==t.length)return{valid:!1,mergeErrorPath:[]};let r=[];for(let n=0;n<e.length;n++){let s=e[n],i=t[n],c=Te(s,i);if(!c.valid)return{valid:!1,mergeErrorPath:[n,...c.mergeErrorPath]};r.push(c.data)}return{valid:!0,data:r}}return{valid:!1,mergeErrorPath:[]}}
function Yr(e,t,r){let n=new Map,s,i=new Map,c=(p,f)=>{let d;if(p.code==="unrecognized_keys"&&!p.path?.length)s??(s=p),d=p.keys;else if(p.code==="invalid_key"&&p.origin==="record"&&p.path?.length===1){let h=String(p.path[0]);if(!i.has(h))i.set(h,p);d=[h]}else return!1;for(let h of d){if(!n.has(h))n.set(h,{});n.get(h)[f]=!0}return!0};for(let p of t.issues)if(!c(p,"l"))e.issues.push(p);for(let p of r.issues)if(!c(p,"r"))e.issues.push(p);let a=[...n].filter(([,p])=>p.l&&p.r).map(([p])=>p);if(a.length){let p=s?a.filter((f)=>s.keys.includes(f)):[];if(p.length)e.issues.push({...s,keys:p});for(let f of a)if(!p.includes(f)&&i.has(f))e.issues.push(i.get(f))}let l=Te(t.value,r.value);if(!l.valid){if(F0t(e))return e;throw Error(`Unmergable intersection. Error path: ${JSON.stringify(l.mergeErrorPath)}`)}return e.value=l.data,e}
var Wo=dr("$ZodRecord",(e,t)=>{ME.init(e,t);let r=sZ.memoizer;r?.attach(e),e._zod.parse=(n,s)=>{let i=n.value;if(!N0t(i))return n.issues.push({expected:"record",code:"invalid_type",input:i,inst:e}),n;let c=[],a=t.keyType._zod.values;if(a&&!t.partial){n.value=r?r.alloc(e,n,{},s):{};let l=new Set;for(let f of a)if(typeof f==="string"||typeof f==="number"||typeof f==="symbol"){if(l.add(typeof f==="number"?f.toString():f),f==="__proto__")continue;let d=t.keyType._zod.run({value:f,issues:[]},s);if(d instanceof Promise)throw Error("Async schemas not supported in object keys currently");if(d.issues.length){n.issues.push({code:"invalid_key",origin:"record",issues:d.issues.map((_)=>mUe(_,s,X8())),input:f,path:[f],inst:e});continue}let h=d.value;if(h==="__proto__")continue;let m=t.valueType._zod.run({value:i[f],issues:[]},s);if(m instanceof Promise)c.push(m.then((_)=>{if(_.issues.length)n.issues.push(...fUe(f,_.issues));n.value[h]=_.value}));else{if(m.issues.length)n.issues.push(...fUe(f,m.issues));n.value[h]=m.value}}let p;for(let f in i)if(!l.has(f))if(t.mode==="loose"){if(f==="__proto__")continue;n.value[f]=i[f]}else p=p??[],p.push(f);if(p&&p.length>0)n.issues.push({code:"unrecognized_keys",input:i,inst:e,keys:p,continue:!0})}else{n.value=r?r.alloc(e,n,{},s):{};let l;for(let p of Reflect.ownKeys(i)){if(p==="__proto__")continue;if(!Object.prototype.propertyIsEnumerable.call(i,p))continue;let f=t.keyType._zod.run({value:p,issues:[]},s);if(f instanceof Promise)throw Error("Async schemas not supported in object keys currently");if(typeof p==="string"&&D.test(p)&&f.issues.length){let _=t.keyType._zod.run({value:Number(p),issues:[]},s);if(_ instanceof Promise)throw Error("Async schemas not supported in object keys currently");if(_.issues.length===0)f=_}if(f.issues.length){if(t.mode==="loose")n.value[p]=i[p];else if(a)l=l??[],l.push(p);else n.issues.push({code:"invalid_key",origin:"record",issues:f.issues.map((_)=>mUe(_,s,X8())),input:p,path:[p],inst:e});continue}let h=f.value;if(h==="__proto__")continue;let m=t.valueType._zod.run({value:i[p],issues:[]},s);if(m instanceof Promise)c.push(m.then((_)=>{if(_.issues.length)n.issues.push(...fUe(p,_.issues));n.value[h]=_.value}));else{if(m.issues.length)n.issues.push(...fUe(p,m.issues));n.value[h]=m.value}}if(l&&l.length>0)n.issues.push({code:"unrecognized_keys",input:i,inst:e,keys:l,continue:!0})}if(c.length)return Promise.all(c).then(()=>n);return n}});
var Go=dr("$ZodEnum",(e,t)=>{ME.init(e,t);let r=GTr(t.entries),n=new Set(r);e._zod.values=n;let s=r.filter((i)=>CPs.has(typeof i));e._zod.pattern=new RegExp(s.length?`^(${s.map((i)=>Jdt(i.toString())).join("|")})$`:"^[^\\s\\S]$"),e._zod.parse=(i,c)=>{let a=i.value;if(n.has(a))return i;return i.issues.push({code:"invalid_value",values:r,input:a,inst:e}),i}});
var Yo=dr("$ZodLiteral",(e,t)=>{ME.init(e,t);let r=new Set(t.values);e._zod.values=r,e._zod.pattern=new RegExp(t.values.length?`^(${t.values.map((n)=>typeof n==="string"?Jdt(n):n?Jdt(n.toString()):String(n)).join("|")})$`:"^[^\\s\\S]$"),e._zod.parse=(n,s)=>{let i=n.value;if(r.has(i))return n;return n.issues.push({code:"invalid_value",values:t.values,input:i,inst:e}),n}});
var qo=dr("$ZodTransform",(e,t)=>{ME.init(e,t),e._zod.optin="optional",sZ.memoizer?.guard(e),e._zod.parse=(r,n)=>{if(n.direction==="backward")throw new aWn(e.constructor.name);let s=t.transform(r.value,r);if(n.async)return(s instanceof Promise?s:Promise.resolve(s)).then((c)=>(r.value=c,r));if(s instanceof Promise)throw new e9e;return r.value=s,r}});
function Qr(e,t){return e.value=t.issues.length?void 0:t.value,e}
var Re=dr("$ZodOptional",(e,t)=>{ME.init(e,t),Jb(e,"optin",(r)=>r.def.innerType._zod.optin==="defaulted"?"defaulted":"optional"),e._zod.optout="optional",Jb(e,"values",(r)=>{let n=r.def.innerType._zod.values;return n?new Set([...n,void 0]):void 0}),Jb(e,"pattern",(r)=>{let n=r.def.innerType._zod.pattern;return n?new RegExp(`^(${Pnn(n.source)})?$`):void 0}),e._zod.parse=(r,n)=>{if(r.value===void 0){if(t.innerType._zod.optin!=="defaulted")return r;let s=t.innerType._zod.run({value:r.value,issues:[]},n);if(s instanceof Promise)return s.then((i)=>Qr(r,i));return Qr(r,s)}return t.innerType._zod.run(r,n)}});
var Xo=dr("$ZodExactOptional",(e,t)=>{Re.init(e,t),Jb(e,"values",(r)=>r.def.innerType._zod.values),Jb(e,"pattern",(r)=>r.def.innerType._zod.pattern),e._zod.parse=(r,n)=>t.innerType._zod.run(r,n)});
var Ho=dr("$ZodNullable",(e,t)=>{ME.init(e,t),Jb(e,"optin",(r)=>r.def.innerType._zod.optin),Jb(e,"optout",(r)=>r.def.innerType._zod.optout),Jb(e,"pattern",(r)=>{let n=r.def.innerType._zod.pattern;return n?new RegExp(`^(${Pnn(n.source)}|null)$`):void 0}),Jb(e,"values",(r)=>r.def.innerType._zod.values?new Set([...r.def.innerType._zod.values,null]):void 0),e._zod.parse=(r,n)=>{if(r.value===null)return r;return t.innerType._zod.run(r,n)}});
var Qo=dr("$ZodDefault",(e,t)=>{ME.init(e,t),e._zod.optin="defaulted",Jb(e,"values",(r)=>r.def.innerType._zod.values),e._zod.parse=(r,n)=>{if(n.direction==="backward")return t.innerType._zod.run(r,n);if(r.value===void 0)return r.value=t.defaultValue,r;let s=t.innerType._zod.run(r,n);if(s instanceof Promise)return s.then((i)=>eo(i,t));return eo(s,t)}});
function eo(e,t){if(e.value===void 0)e.value=t.defaultValue;return e}
var en=dr("$ZodPrefault",(e,t)=>{ME.init(e,t),e._zod.optin="defaulted",Jb(e,"values",(r)=>r.def.innerType._zod.values),e._zod.parse=(r,n)=>{if(n.direction==="backward")return t.innerType._zod.run(r,n);if(r.value===void 0)r.value=t.defaultValue;return t.innerType._zod.run(r,n)}});
var tn=dr("$ZodNonOptional",(e,t)=>{ME.init(e,t),Jb(e,"values",(r)=>{let n=r.def.innerType._zod.values;return n?new Set([...n].filter((s)=>s!==void 0)):void 0}),e._zod.parse=(r,n)=>{let s=t.innerType._zod.run(r,n);if(s instanceof Promise)return s.then((i)=>to(i,e));return to(s,e)}});
function to(e,t){if(!e.issues.length&&e.value===void 0)e.issues.push({code:"invalid_type",expected:"nonoptional",input:e.value,inst:t});return e}
function ro(e,t,r,n){if(!t.issues.length){if(e.value=t.value,t.memo)e.memo=!0;return e}return e.value=r.catchValue({...t,value:e.value,error:{issues:t.issues.map((s)=>mUe(s,n,X8()))},input:e.value}),e}
var rn=dr("$ZodCatch",(e,t)=>{ME.init(e,t),Jb(e,"optin",(r)=>r.def.innerType._zod.optin==="defaulted"?"defaulted":"optional"),Jb(e,"optout",(r)=>r.def.innerType._zod.optout),Jb(e,"values",(r)=>r.def.innerType._zod.values),e._zod.parse=(r,n)=>{if(n.direction==="backward")return t.innerType._zod.run(r,n);let s=t.innerType._zod.run({value:r.value,issues:[]},n);if(s instanceof Promise)return s.then((i)=>ro(r,i,t,n));return ro(r,s,t,n)}});
var De=dr("$ZodPipe",(e,t)=>{ME.init(e,t),Jb(e,"values",(r)=>r.def.in._zod.values),Jb(e,"optin",(r)=>r.def.in._zod.optin),Jb(e,"optout",(r)=>r.def.out._zod.optout),Jb(e,"propValues",(r)=>r.def.in._zod.propValues),e._zod.parse=(r,n)=>{if(n.direction==="backward"){let i=t.out._zod.run(r,n);if(i instanceof Promise)return i.then((c)=>ce(c,t.in,n));return ce(i,t.in,n)}let s=t.in._zod.run(r,n);if(s instanceof Promise)return s.then((i)=>ce(i,t.out,n));return ce(s,t.out,n)}});
function ce(e,t,r){if(e.issues.some((n)=>n.code!=="unrecognized_keys"))return e.aborted=!0,e;return t._zod.run({value:e.value,issues:e.issues},r)}
var on=dr("$ZodPreprocess",(e,t)=>{De.init(e,t)});
var nn=dr("$ZodReadonly",(e,t)=>{ME.init(e,t),Jb(e,"propValues",(r)=>r.def.innerType._zod.propValues),Jb(e,"values",(r)=>r.def.innerType._zod.values),Jb(e,"optin",(r)=>r.def.innerType?._zod?.optin),Jb(e,"optout",(r)=>r.def.innerType?._zod?.optout),e._zod.parse=(r,n)=>{if(n.direction==="backward")return t.innerType._zod.run(r,n);let s=t.innerType._zod.run(r,n);if(s instanceof Promise)return s.then(oo);return oo(s)}});
function oo(e){if(!e.memo)e.value=Object.freeze(e.value);return e}
var sn=dr("$ZodCustom",(e,t)=>{Z.init(e,t),ME.init(e,t),e._zod.parse=(r,n)=>r,e._zod.check=(r)=>{let n=r.value,s=t.fn(n);if(s instanceof Promise)return s.then((i)=>no(i,r,n,e));no(s,r,n,e);return}});
function no(e,t,r,n){if(!e){let s={code:"custom",input:r,inst:n,path:[...n._zod.def.path??[]],continue:!n._zod.def.abort};if(n._zod.def.params)s.params=n._zod.def.params;t.issues.push($0t(s))}}
function cn(e,t){return new e({type:"string",...kl(t)})}
function fPs(e,t){return new e({type:"string",coerce:!0,...kl(t)})}
function je(e,t){return new e({type:"string",format:"email",check:"string_format",abort:!1,...kl(t)})}
function un(e,t){return new e({type:"string",format:"guid",check:"string_format",abort:!1,...kl(t)})}
function Le(e,t){return new e({type:"string",format:"uuid",check:"string_format",abort:!1,...kl(t)})}
function an(e,t){return new e({type:"string",format:"uuid",check:"string_format",abort:!1,version:"v4",...kl(t)})}
function ln(e,t){return new e({type:"string",format:"uuid",check:"string_format",abort:!1,version:"v6",...kl(t)})}
function pn(e,t){return new e({type:"string",format:"uuid",check:"string_format",abort:!1,version:"v7",...kl(t)})}
function Fe(e,t){return new e({type:"string",format:"url",check:"string_format",abort:!1,...kl(t)})}
function fn(e,t){return new e({type:"string",format:"emoji",check:"string_format",abort:!1,...kl(t)})}
function dn(e,t){return new e({type:"string",format:"nanoid",check:"string_format",abort:!1,...kl(t)})}
function mn(e,t){return new e({type:"string",format:"cuid",check:"string_format",abort:!1,...kl(t)})}
function hn(e,t){return new e({type:"string",format:"cuid2",check:"string_format",abort:!1,...kl(t)})}
function _n(e,t){return new e({type:"string",format:"ulid",check:"string_format",abort:!1,...kl(t)})}
function gn(e,t){return new e({type:"string",format:"xid",check:"string_format",abort:!1,...kl(t)})}
function xn(e,t){return new e({type:"string",format:"ksuid",check:"string_format",abort:!1,...kl(t)})}
function vn(e,t){return new e({type:"string",format:"ipv4",check:"string_format",abort:!1,...kl(t)})}
function zn(e,t){return new e({type:"string",format:"ipv6",check:"string_format",abort:!1,...kl(t)})}
function yn(e,t){return new e({type:"string",format:"cidrv4",check:"string_format",abort:!1,...kl(t)})}
function $n(e,t){return new e({type:"string",format:"cidrv6",check:"string_format",abort:!1,...kl(t)})}
function Ue(e,t){return new e({type:"string",format:"base64",check:"string_format",abort:!1,...kl(t)})}
function bn(e,t){return new e({type:"string",format:"base64url",check:"string_format",abort:!1,...kl(t)})}
function kn(e,t){return new e({type:"string",format:"e164",check:"string_format",abort:!1,...kl(t)})}
function Zn(e,t){return new e({type:"string",format:"jwt",check:"string_format",abort:!1,...kl(t)})}
function UTr(e,t){return new e({type:"string",format:"datetime",check:"string_format",offset:!1,local:!1,precision:null,...kl(t)})}
function BTr(e,t){return new e({type:"string",format:"date",check:"string_format",...kl(t)})}
function wn(e,t){return new e({type:"string",format:"time",check:"string_format",precision:null,...kl(t)})}
function Pn(e,t){return new e({type:"string",format:"duration",check:"string_format",...kl(t)})}
function Sn(e,t){return new e({type:"number",checks:[],...kl(t)})}
function In(e,t){return new e({type:"number",check:"number_format",abort:!1,format:"safeint",...kl(t)})}
function Tn(e,t){return new e({type:"boolean",...kl(t)})}
function An(e,t){return new e({type:"undefined",...kl(t)})}
function En(e,t){return new e({type:"null",...kl(t)})}
function Cn(e){return new e({type:"unknown"})}
function Rn(e,t){return new e({type:"never",...kl(t)})}
function le(e,t){return new ke({check:"less_than",...kl(t),value:e,inclusive:!1})}
function J(e,t){return new ke({check:"less_than",...kl(t),value:e,inclusive:!0})}
function fe(e,t){return new Ze({check:"greater_than",...kl(t),value:e,inclusive:!1})}
function B(e,t){return new Ze({check:"greater_than",...kl(t),value:e,inclusive:!0})}
function de(e,t){return new _r({check:"multiple_of",...kl(t),value:e})}
function Tnn(e,t){return new xr({check:"max_length",...kl(t),maximum:e})}
function R(e,t){return new vr({check:"min_length",...kl(t),minimum:e})}
function me(e,t){return new zr({check:"length_equals",...kl(t),length:e})}
function Me(e,t){return new yr({check:"string_format",format:"regex",...kl(t),pattern:e})}
function Je(e){return new $r({check:"string_format",format:"lowercase",...kl(e)})}
function Ve(e){return new br({check:"string_format",format:"uppercase",...kl(e)})}
function Ke(e,t){return new kr({check:"string_format",format:"includes",...kl(t),includes:e})}
function We(e,t){return new Zr({check:"string_format",format:"starts_with",...kl(t),prefix:e})}
function Ge(e,t){return new wr({check:"string_format",format:"ends_with",...kl(t),suffix:e})}
function E(e){return new Pr({check:"overwrite",tx:e})}
function Ye(e){return E((t)=>t.normalize(e))}
function qe(){return E((e)=>e.trim())}
function Xe(){return E((e)=>e.toLowerCase())}
function He(){return E((e)=>e.toUpperCase())}
function Qe(){return E((e)=>vPs(e))}
function Dn(e,t,r){return new e({type:"array",element:t,...kl(r)})}
function jn(e,t,r){let n=kl(r);return n.abort??(n.abort=!0),new e({type:"custom",check:"custom",fn:t,...n})}
function Ln(e,t,r){return new e({type:"custom",check:"custom",fn:t,...kl(r)})}
function Fn(e,t){let r=si((n)=>(n.addIssue=(s)=>{if(typeof s==="string")n.issues.push($0t(s,n.value,r._zod.def));else{let i=s;if(i.fatal)i.continue=!1;if(i.code??(i.code="custom"),!("input"in i))i.input=n.value;i.inst??(i.inst=r),i.continue??(i.continue=!r._zod.def.abort),n.issues.push($0t(i))}},e(n.value,n)),t);return r}
function si(e,t){let r=new Z({check:"custom",...kl(t)});return r._zod.check=e,r}
class Jn extends Error{constructor(){super("Cannot parse a reference cycle that closes through a transform");this.name="ZodCyclicError"}}
var tt="~memo";
var Un=[];
function et(e){return e.map((t)=>t.path?{...t,path:t.path.slice()}:{...t})}
var Mn=new WeakMap;
function Bn(e,t){let r=Mn.get(e);if(r!==void 0)return r;if(t.has(e))return!0;t.add(e);let n=!1,s=(a)=>{if(!n&&a?._zod&&Bn(a,t))n=!0},i=e._zod.def,c=i.type;switch(c){case"object":{for(let a of Reflect.ownKeys(i.shape))s(i.shape[a]);s(i.catchall);break}case"array":s(i.element);break;case"tuple":for(let a of i.items)s(a);s(i.rest);break;case"record":case"map":s(i.keyType),s(i.valueType);break;case"set":s(i.valueType);break;case"union":for(let a of i.options)s(a);break;case"intersection":s(i.left),s(i.right);break;case"optional":case"nullable":case"default":case"prefault":case"catch":case"readonly":case"nonoptional":case"promise":case"success":s(i.innerType);break;case"pipe":s(i.in),s(i.out);break;case"function":s(i.input),s(i.output);break;case"lazy":s(e._zod.innerType);break;case"template_literal":case"string":case"number":case"int":case"boolean":case"bigint":case"symbol":case"undefined":case"null":case"void":case"never":case"any":case"unknown":case"date":case"nan":case"enum":case"literal":case"file":case"transform":case"custom":break;default:for(let a in i){let l=Object.getOwnPropertyDescriptor(i,a);if(!l||l.get)continue;let p=l.value;if(!p||typeof p!=="object")continue;if(p._zod)s(p);else if(Array.isArray(p))for(let f of p)s(f)}}return t.delete(e),Mn.set(e,n),n}
function ii(e,t){let r=e.buckets.get(t);if(!r)r=new Map,e.buckets.set(t,r);return r}
var he;
var _e=[];
var ci={alloc(e,t,r){let n=he;if(!n)return r;he=void 0;let s={value:r,issues:null};return n.set(t.value,s),_e.push(s),r},guard(e){var t;(t=e._zod).deferred??(t.deferred=[]),e._zod.deferred.push(()=>{let r=e._zod.parse,n=(s,i)=>{if(i.direction!=="backward"&&ui(i,s.value))throw new Jn;return r(s,i)};if(e._zod.parse=n,e._zod.run===r)e._zod.run=n})},attach(e){var t;let r,n,s;(t=e._zod).deferred??(t.deferred=[]),e._zod.deferred.push(()=>{let i=e._zod.parse,c=(a,l)=>{if(r===void 0){if(r=Bn(e,new Set),!r){if(e._zod.parse=i,e._zod.run===c)e._zod.run=i;return i(a,l)}}let p=a.value;if(p===null||typeof p!=="object")return i(a,l);let f=l[tt];if(!f)f={buckets:new Map,backEdges:void 0},l[tt]=f;let d;if(n===l)d=s;else d=ii(f,e),n=l,s=d;let h=d.get(p);if(h){if(a.value=h.value,h.issues){if(h.issues.length)a.issues.push(...et(h.issues))}else a.memo=!0,f.backEdges??(f.backEdges=new Set),f.backEdges.add(h.value);return a}he=d;let m=_e.length,_=i(a,l);he=void 0;let x=_e.length>m?_e.pop():void 0;if(_ instanceof Promise)return _.then((g)=>{if(x)x.issues=g.issues.length?et(g.issues):Un;return g});if(x)x.issues=_.issues.length?et(_.issues):Un;return _};if(e._zod.parse=c,e._zod.run===i)e._zod.run=c})}};
function Vn(){return ci}
function ui(e,t){let r=e[tt]?.backEdges;return r!==void 0&&t!==null&&typeof t==="object"&&r.has(t)}
var ai=()=>{let e={string:{unit:"characters",verb:"to have"},file:{unit:"bytes",verb:"to have"},array:{unit:"items",verb:"to have"},set:{unit:"items",verb:"to have"},map:{unit:"entries",verb:"to have"}};function t(i){return e[i]??null}let r={regex:"input",email:"email address",url:"URL",emoji:"emoji",uuid:"UUID",uuidv4:"UUIDv4",uuidv6:"UUIDv6",nanoid:"nanoid",guid:"GUID",cuid:"cuid",cuid2:"cuid2",ulid:"ULID",xid:"XID",ksuid:"KSUID",datetime:"ISO datetime",date:"ISO date",time:"ISO time",duration:"ISO duration",ipv4:"IPv4 address",ipv6:"IPv6 address",mac:"MAC address",cidrv4:"IPv4 range",cidrv6:"IPv6 range",base64:"base64-encoded string",base64url:"base64url-encoded string",json_string:"JSON string",e164:"E.164 number",credit_card:"credit card number",jwt:"JWT",template_literal:"input"},n={nan:"NaN"};function s(i,c){if(i==="number"&&typeof c==="number"&&!Number.isFinite(c))return String(c);return n[i]??i}return(i)=>{switch(i.code){case"invalid_type":{let c=s(i.expected),a=PPs(i.input),l=s(a,i.input);return`Invalid input: expected ${c}, received ${l}`}case"invalid_value":if(i.values.length===1)return`Invalid input: expected ${qCo(i.values[0])}`;return`Invalid option: expected one of ${GCo(i.values,"|")}`;case"too_big":{let c=i.exact?"exactly ":i.inclusive?"<=":"<",a=t(i.origin);if(a)return`Too big: expected ${i.origin??"value"} to have ${c}${i.maximum.toString()} ${a.unit??"elements"}`;return`Too big: expected ${i.origin??"value"} to be ${c}${i.maximum.toString()}`}case"too_small":{let c=i.exact?"exactly ":i.inclusive?">=":">",a=t(i.origin);if(a)return`Too small: expected ${i.origin} to have ${c}${i.minimum.toString()} ${a.unit}`;return`Too small: expected ${i.origin} to be ${c}${i.minimum.toString()}`}case"invalid_format":{let c=i;if(c.format==="starts_with")return`Invalid string: must start with "${c.prefix}"`;if(c.format==="ends_with")return`Invalid string: must end with "${c.suffix}"`;if(c.format==="includes")return`Invalid string: must include "${c.includes}"`;if(c.format==="regex")return`Invalid string: must match pattern ${c.pattern}`;return`Invalid ${r[c.format]??i.format}`}case"not_multiple_of":return`Invalid number: must be a multiple of ${i.divisor}`;case"unrecognized_keys":return`Unrecognized key${i.keys.length>1?"s":""}: ${GCo(i.keys,", ")}`;case"invalid_key":return`Invalid key in ${i.origin}`;case"invalid_union":if(i.options&&Array.isArray(i.options)&&i.options.length>0)return`Invalid discriminator value. Expected ${i.options.map((a)=>`'${a}'`).join(" | ")}`;if(i.inclusive===!1)return"Invalid input: more than one option matched";return"Invalid input";case"invalid_element":return`Invalid value in ${i.origin}`;default:return"Invalid input"}}};
function rt(){return{localeError:ai()}}
function V(e,...t){for(let r of t)for(let n of Reflect.ownKeys(r))if(Object.prototype.propertyIsEnumerable.call(r,n))zW(e,n,r[n]);return e}
function W(e){let t=e?.target??"draft-2020-12";if(t==="draft-4")t="draft-04";if(t==="draft-7")t="draft-07";return{processors:e.processors??{},metadataRegistry:e?.metadata??Q5e,target:t,unrepresentable:e?.unrepresentable??"throw",override:e?.override??(()=>{}),io:e?.io??"output",counter:0,seen:new Map,sharedDefsExtractedFor:void 0,sharedEmitDoneFor:void 0,cycles:e?.cycles??"ref",reused:e?.reused??"inline",intersections:[],deferred:[],external:e?.external??void 0}}
function k(e,t,r,n,s){let i=typeof t.unrepresentable==="function"?t.unrepresentable({zodSchema:e,path:n.path,message:s}):t.unrepresentable;if(i==="any")return!1;if(i===void 0||i==="throw")throw Error(s);return Object.assign(r,i),!0}
function KC(e,t,r={path:[],schemaPath:[]}){var n;let s=e._zod.def,i=t.seen.get(e);if(i){if(i.count++,r.schemaPath.includes(e))i.cycle=r.path;return i.schema}let c={schema:{},count:1,cycle:void 0,path:r.path};t.seen.set(e,c),t.sharedDefsExtractedFor=void 0,t.sharedEmitDoneFor=void 0;let a=e._zod.toJSONSchema?.();if(a)c.schema=a;else{let f={...r,schemaPath:[...r.schemaPath,e],path:r.path};if(e._zod.processJSONSchema)e._zod.processJSONSchema(t,c.schema,f);else{let h=c.schema,m=t.processors[s.type];if(!m)throw Error(`[toJSONSchema]: Non-representable type encountered: ${s.type}`);m(e,t,h,f)}let d=e._zod.parent;if(d){if(!c.ref)c.ref=d;KC(d,t,f),t.seen.get(d).isParent=!0}}let l=t.metadataRegistry.get(e);if(l)V(c.schema,l);if(t.io==="input"&&w(e))delete c.schema.examples,delete c.schema.default;if(t.io==="input"&&"_prefault"in c.schema)(n=c.schema).default??(n.default=c.schema._prefault);return delete c.schema._prefault,t.seen.get(e).schema}
function Kn(e){return e.replace(/~/g,"~0").replace(/\//g,"~1")}
function G(e,t){let r=e.seen.get(t);if(!r)throw Error("Unprocessed schema. This is a bug in Zod.");if(e.external&&e.sharedDefsExtractedFor===e.external)return;let n=new Map;for(let c of e.seen.entries()){let a=e.metadataRegistry.get(c[0])?.id;if(a){let l=n.get(a);if(l&&l!==c[0])throw Error(`Duplicate schema id "${a}" detected during JSON Schema conversion. Two different schemas cannot share the same id when converted together.`);n.set(a,c[0])}}let s=(c)=>{let a=e.target==="draft-2020-12"?"$defs":"definitions";if(e.external){let d=e.external.registry.get(c[0])?.id,h=e.external.uri??((_)=>_);if(d)return{ref:h(d)};let m=c[1].defId??c[1].schema.id??`schema${e.counter++}`;return c[1].defId=m,{defId:m,ref:`${h("__shared")}#/${a}/${Kn(m)}`}}let l="#",p=`${l}/${a}/`;if(c[1]===r&&!c[1].schema.id)return{ref:l};let f=c[1].schema.id??`__schema${e.counter++}`;return{defId:f,ref:p+Kn(f)}},i=(c)=>{if(c[1].schema.$ref)return;let a=c[1],{ref:l,defId:p}=s(c);if(a.def={...a.schema},p)a.defId=p;let f=a.schema;for(let d in f)delete f[d];f.$ref=l};if(e.cycles==="throw")for(let c of e.seen.entries()){let a=c[1];if(a.cycle)throw Error(`Cycle detected: #/${a.cycle?.join("/")}/<root>

Set the \`cycles\` parameter to \`"ref"\` to resolve cyclical schemas with defs.`)}for(let c of e.seen.entries()){let a=c[1];if(t===c[0]){i(c);continue}if(e.external){let p=e.external.registry.get(c[0])?.id;if(t!==c[0]&&p){i(c);continue}}if(e.metadataRegistry.get(c[0])?.id){i(c);continue}if(a.cycle){i(c);continue}if(a.count>1){if(e.reused==="ref"){i(c);continue}}}if(e.external)e.sharedDefsExtractedFor=e.external}
function Yn(e){let t=e.anyOf;if(!Array.isArray(t)||t.length===0||e.type!==void 0)return;let r=[];for(let n of t){if(!n||typeof n!=="object")return;Yn(n);let s=Object.keys(n);if(s.length!==1||s[0]!=="type")return;let i=n.type;for(let c of Array.isArray(i)?i:[i]){if(typeof c!=="string")return;if(!r.includes(c))r.push(c)}}delete e.anyOf,e.type=r.length===1?r[0]:r}
var qn=new Set(["type","properties","required","additionalProperties"]);
var Wn=["oneOf","anyOf"];
function Gn(e){let t=e.additionalProperties;if(t===void 0||t===!1||typeof t!=="object"||t===null)return null;return Object.keys(t).length?t:null}
function ot(e){let t=[];for(let i of e){if(typeof i!=="object"||i.type!=="object")return null;for(let c in i)if(!qn.has(c))return null;t.push(i)}let r={},n=new Set;for(let i of t){for(let c in i.properties){if(Object.prototype.hasOwnProperty.call(r,c))continue;let a=[];for(let p of t){let f=p.properties?.[c]??Gn(p);if(f===null||f===void 0)continue;if(!a.some((d)=>JSON.stringify(d)===JSON.stringify(f)))a.push(f)}let l=a.length===1?a[0]:ot(a)??{allOf:a};zW(r,c,l)}for(let c of i.required??[])n.add(c)}let s={type:"object",properties:r};if(n.size)s.required=[...n];if(t.every((i)=>i.additionalProperties===!1))s.additionalProperties=!1;else{let i=[];for(let c of t){let a=Gn(c);if(a&&!i.some((l)=>JSON.stringify(l)===JSON.stringify(a)))i.push(a)}if(i.length===1)s.additionalProperties=i[0];else if(i.length>1)s.additionalProperties={allOf:i}}return s}
function li(e){let t=e.allOf;if(!Array.isArray(t)||t.length<2)return;for(let s of qn)if(s in e)return;let r=t.filter((s)=>Wn.some((i)=>Array.isArray(s[i]))),n=null;if(!r.length)n=ot(t);else{let s=r[0],i=Wn.find((l)=>Array.isArray(s[l]));if(Object.keys(s).length!==1)return;let c=t.filter((l)=>l!==s),a=s[i].map((l)=>ot([...c,l]));if(a.some((l)=>!l))return;n={[i]:a}}if(!n)return;delete e.allOf,V(e,n)}
function Y(e,t){let r=e.seen.get(t);if(!r)throw Error("Unprocessed schema. This is a bug in Zod.");let n=(a)=>{let l=e.seen.get(a);if(l.ref===null)return;let p=l.def??l.schema,f={...p},d=l.ref;if(l.ref=null,d){n(d);let m=e.seen.get(d),_=m.schema;if(_.$ref&&(e.target==="draft-07"||e.target==="draft-04"||e.target==="openapi-3.0"))p.allOf=p.allOf??[],p.allOf.push(_);else V(p,_);if(V(p,f),a._zod.parent===d)for(let g in p){if(g==="$ref"||g==="allOf")continue;if(!(g in f))delete p[g]}if(_.$ref&&m.def)for(let g in p){if(g==="$ref"||g==="allOf")continue;if(g in m.def&&JSON.stringify(p[g])===JSON.stringify(m.def[g]))delete p[g]}}let h=a._zod.parent;if(h&&h!==d){n(h);let m=e.seen.get(h);if(m?.schema.$ref){if(p.$ref=m.schema.$ref,m.def)for(let _ in p){if(_==="$ref"||_==="allOf")continue;if(_ in m.def&&JSON.stringify(p[_])===JSON.stringify(m.def[_]))delete p[_]}}}e.override({zodSchema:a,jsonSchema:p,path:l.path??[]})};if(!e.external||e.sharedEmitDoneFor!==e.external){for(let a of[...e.seen.entries()].reverse())n(a[0]);if(e.target!=="openapi-3.0")for(let a of e.seen.entries())Yn(a[1].def??a[1].schema);for(let a of e.deferred)a();if(e.intersections.length){let a=new Map;for(let l of e.seen.values())for(let p of[l.schema,l.def]){let f=p?.allOf;if(!Array.isArray(f))continue;let d=a.get(f);if(d)d.push(p);else a.set(f,[p])}for(let l of e.intersections)for(let p of a.get(l)??[])li(p)}}let s={};if(e.target==="draft-2020-12")s.$schema="https://json-schema.org/draft/2020-12/schema";else if(e.target==="draft-07")s.$schema="http://json-schema.org/draft-07/schema#";else if(e.target==="draft-04")s.$schema="http://json-schema.org/draft-04/schema#";else if(e.target==="openapi-3.0");if(e.external?.uri){let a=e.external.registry.get(t)?.id;if(!a)throw Error("Schema is missing an `id` property");s.$id=e.external.uri(a)}V(s,r.defId?r.schema:r.def??r.schema);let i=e.metadataRegistry.get(t)?.id;if(i!==void 0&&s.id===i)delete s.id;let c=e.external?.defs??{};if(!e.external||e.sharedEmitDoneFor!==e.external)for(let a of e.seen.entries()){let l=a[1];if(l.def&&l.defId){if(l.def.id===l.defId)delete l.def.id;zW(c,l.defId,l.def)}}if(e.external)e.sharedEmitDoneFor=e.external;if(e.external);else if(Object.keys(c).length>0)if(e.target==="draft-2020-12")s.$defs=c;else s.definitions=c;try{let a=JSON.parse(JSON.stringify(s));return Object.defineProperty(a,"~standard",{value:{...t["~standard"],jsonSchema:{input:K(t,"input",e.processors),output:K(t,"output",e.processors)}},enumerable:!1,writable:!1}),a}catch(a){throw Error("Error converting schema to JSON.")}}
function w(e,t){let r=t??{seen:new Set};if(r.seen.has(e))return!1;r.seen.add(e);let n=e._zod.def;if(n.type==="transform")return!0;if(n.type==="array")return w(n.element,r);if(n.type==="set")return w(n.valueType,r);if(n.type==="lazy")return w(n.getter(),r);if(n.type==="promise"||n.type==="optional"||n.type==="nonoptional"||n.type==="nullable"||n.type==="readonly"||n.type==="default"||n.type==="prefault"||n.type==="catch")return w(n.innerType,r);if(n.type==="intersection")return w(n.left,r)||w(n.right,r);if(n.type==="record"||n.type==="map")return w(n.keyType,r)||w(n.valueType,r);if(n.type==="pipe"){if(e._zod.traits.has("$ZodCodec"))return!0;return w(n.in,r)||w(n.out,r)}if(n.type==="object"){for(let s in n.shape)if(w(n.shape[s],r))return!0;return!1}if(n.type==="union"){for(let s of n.options)if(w(s,r))return!0;return!1}if(n.type==="tuple"){for(let s of n.items)if(w(s,r))return!0;if(n.rest&&w(n.rest,r))return!0;return!1}return!1}
var Xn=(e,t={})=>(r)=>{let n=W({...r,processors:t});return KC(e,n),G(n,e),Y(n,e)};
var K=(e,t,r={})=>(n)=>{let{libraryOptions:s,target:i}=n??{},c=W({...s??{},target:i,io:t,processors:r});return KC(e,c),G(c,e),Y(c,e)};
var pi={guid:"uuid",url:"uri",datetime:"date-time",json_string:"json-string",regex:""};
var it=(e,t,r,n)=>{let s=r;s.type="string";let{minimum:i,maximum:c,format:a,patterns:l,contentEncoding:p,laxFormat:f}=e._zod.bag;if(typeof i==="number")s.minLength=i;if(typeof c==="number")s.maxLength=c;if(a){if(s.format=pi[a]??a,s.format==="")delete s.format;if(a==="time"||f)delete s.format}if(p)s.contentEncoding=p;if(l&&l.size>0){let d=[...l];if(d.length===1)s.pattern=d[0].source;else if(d.length>1)s.allOf=[...d.map((h)=>({...t.target==="draft-07"||t.target==="draft-04"||t.target==="openapi-3.0"?{type:"string"}:{},pattern:h.source}))]}};
var ct=(e,t,r,n)=>{let s=r,{minimum:i,maximum:c,format:a,multipleOf:l,exclusiveMaximum:p,exclusiveMinimum:f}=e._zod.bag;if(typeof a==="string"&&a.includes("int"))s.type="integer";else s.type="number";let d=typeof f==="number"&&f>=(i??Number.NEGATIVE_INFINITY),h=typeof p==="number"&&p<=(c??Number.POSITIVE_INFINITY),m=t.target==="draft-04"||t.target==="openapi-3.0";if(d)if(m)s.minimum=f,s.exclusiveMinimum=!0;else s.exclusiveMinimum=f;else if(typeof i==="number")s.minimum=i;if(h)if(m)s.maximum=p,s.exclusiveMaximum=!0;else s.exclusiveMaximum=p;else if(typeof c==="number")s.maximum=c;if(typeof l==="number")if(Number.isFinite(l)&&l!==0)s.multipleOf=Math.abs(l);else k(e,t,s,n,`A multipleOf divisor of ${l} cannot be represented in JSON Schema`)};
var ut=(e,t,r,n)=>{r.type="boolean"};
var at=(e,t,r,n)=>{if(t.target==="openapi-3.0")r.type="string",r.nullable=!0,r.enum=[null];else r.type="null"};
var lt=(e,t,r,n)=>{k(e,t,r,n,"Undefined cannot be represented in JSON Schema")};
var pt=(e,t,r,n)=>{r.not={}};
var dt=(e,t,r,n)=>{};
var mt=(e,t,r,n)=>{let s=e._zod.def,i=GTr(s.entries);if(i.length===0){r.not={};return}if(i.every((c)=>typeof c==="number"))r.type="number";if(i.every((c)=>typeof c==="string"))r.type="string";r.enum=i};
var ht=(e,t,r,n)=>{let s=e._zod.def;if(s.values.length===0){r.not={};return}let i=[];for(let c of s.values)if(c===void 0){if(k(e,t,r,n,"Literal `undefined` cannot be represented in JSON Schema"))return}else if(typeof c==="bigint"){if(k(e,t,r,n,"BigInt literals cannot be represented in JSON Schema"))return;i.push(Number(c))}else i.push(c);if(i.length===0);else if(i.length===1){let c=i[0];if(r.type=c===null?"null":typeof c,t.target==="draft-04"||t.target==="openapi-3.0")r.enum=[c];else r.const=c}else{if(i.every((c)=>typeof c==="number"))r.type="number";if(i.every((c)=>typeof c==="string"))r.type="string";if(i.every((c)=>typeof c==="boolean"))r.type="boolean";if(i.every((c)=>c===null))r.type="null";r.enum=i}};
var gt=(e,t,r,n)=>{k(e,t,r,n,"Custom types cannot be represented in JSON Schema")};
var xt=(e,t,r,n)=>{k(e,t,r,n,"Transforms cannot be represented in JSON Schema")};
var vt=(e,t,r,n)=>{let s=r,i=e._zod.def,{minimum:c,maximum:a}=e._zod.bag;if(typeof c==="number")s.minItems=c;if(typeof a==="number")s.maxItems=a;s.type="array",s.items=KC(i.element,t,{...n,path:[...n.path,"items"]})};
function q(e){let t=e._zod.def;if(t.type==="pipe"&&t.in._zod.traits.has("$ZodTransform"))return q(t.out);if(t.type==="catch")return q(t.innerType);return e._zod.optin}
var zt=(e,t,r,n)=>{let s=r,i=e._zod.def,c=i.shape;if(Object.getOwnPropertySymbols(c).length&&k(e,t,s,n,"Symbol keys cannot be represented in JSON Schema"))return;s.type="object",s.properties={};for(let f in c)zW(s.properties,f,KC(c[f],t,{...n,path:[...n.path,"properties",f]}));let l=new Set(Object.keys(c)),p=new Set([...l].filter((f)=>{let d=i.shape[f];if(t.io==="input")return q(d)===void 0;else return d._zod.optout===void 0}));if(p.size>0)s.required=Array.from(p);if(i.catchall?._zod.def.type==="never")s.additionalProperties=!1;else if(!i.catchall){if(t.io==="output")s.additionalProperties=!1}else if(i.catchall)s.additionalProperties=KC(i.catchall,t,{...n,path:[...n.path,"additionalProperties"]})};
var yt=(e,t,r,n)=>{let s=e._zod.def,i=s.inclusive===!1,c=s.options.map((a,l)=>KC(a,t,{...n,path:[...n.path,i?"oneOf":"anyOf",l]}));if(i)r.oneOf=c;else r.anyOf=c};
var $t=(e,t,r,n)=>{let s=e._zod.def,i=KC(s.left,t,{...n,path:[...n.path,"allOf",0]}),c=KC(s.right,t,{...n,path:[...n.path,"allOf",1]}),a=(p)=>("allOf"in p)&&Object.keys(p).length===1,l=[...a(i)?i.allOf:[i],...a(c)?c.allOf:[c]];r.allOf=l,t.intersections.push(l)};
function nt(e,t,r){if(t.$ref){if(r.has(t))return t;r.add(t);let _=e.get(t)?.def;if(!_)return t;let x=nt(e,_,r);return x===_?t:x}for(let _ of["anyOf","oneOf"]){let x=t[_];if(!Array.isArray(x))continue;let g=x.map((O)=>nt(e,O,r));if(g.some((O,N)=>O!==x[N]))t={...t,[_]:g}}let n=Array.isArray(t.type)?t.type:[t.type],s=!n.includes("string")&&n.some((_)=>_==="number"||_==="integer"),i=t.enum??(t.const!==void 0?[t.const]:void 0);if(!s&&!i?.some((_)=>typeof _==="number"))return t;let{minimum:c,maximum:a,exclusiveMinimum:l,exclusiveMaximum:p,multipleOf:f,format:d,id:h,...m}=t;if(m.enum)m.enum=m.enum.map((_)=>typeof _==="number"?String(_):_);else if(typeof m.const==="number")m.const=String(m.const);if(!s)return m;if(m.type="string",!i)m.pattern=(n.includes("number")?D:ee).source;return m}
var st=new WeakMap;
function fi(e){let t=new Map;for(let n of e.seen.values())if(n.def&&!t.has(n.schema))t.set(n.schema,n);let r=new Map;for(let n of st.get(e)??[]){let s=e.seen.get(n),i=(s?.def??s?.schema)?.propertyNames;if(!i||i===!0||r.has(i))continue;let c=nt(t,i,new Set);if(c!==i)r.set(i,c)}if(!r.size)return;for(let n of e.seen.values())for(let s of[n.schema,n.def]){let i=s&&r.get(s.propertyNames);if(i)s.propertyNames=i}}
var kt=(e,t,r,n)=>{let s=r,i=e._zod.def;s.type="object";let c=i.keyType,l=c._zod.bag?.patterns;if(i.mode==="loose"&&l&&l.size>0){let d=KC(i.valueType,t,{...n,path:[...n.path,"patternProperties","*"]});s.patternProperties={};for(let h of l)zW(s.patternProperties,h.source,d)}else{if(t.target==="draft-07"||t.target==="draft-2020-12"){s.propertyNames=KC(i.keyType,t,{...n,path:[...n.path,"propertyNames"]});let d=st.get(t);if(!d)d=[],st.set(t,d),t.deferred.push(()=>fi(t));d.push(e)}s.additionalProperties=KC(i.valueType,t,{...n,path:[...n.path,"additionalProperties"]})}let p=c._zod.values,f=t.io==="input"&&q(i.valueType)!==void 0;if(p&&!i.partial&&!f){let d=[...p].filter((h)=>typeof h==="string"||typeof h==="number");if(d.length>0)s.required=d.map(String)}};
var Zt=(e,t,r,n)=>{let s=e._zod.def,i=KC(s.innerType,t,n),c=t.seen.get(e);if(t.target==="openapi-3.0")c.ref=s.innerType,r.nullable=!0;else r.anyOf=[i,{type:"null"}]};
var wt=(e,t,r,n)=>{let s=e._zod.def;KC(s.innerType,t,n);let i=t.seen.get(e);i.ref=s.innerType};
var Pt=Symbol();
function ls(e,t,r,n,s){let i=!1,c=JSON.stringify(e,(a,l)=>{if(typeof l!=="bigint")return l;return i=!0,null});if(!i)return JSON.parse(c);return k(t,r,n,s,"BigInt defaults cannot be represented in JSON Schema"),Pt}
var St=(e,t,r,n)=>{let s=e._zod.def;KC(s.innerType,t,n);let i=t.seen.get(e);i.ref=s.innerType;let c=ls(s.defaultValue,e,t,r,n);if(c!==Pt)r.default=c};
var It=(e,t,r,n)=>{let s=e._zod.def;KC(s.innerType,t,n);let i=t.seen.get(e);if(i.ref=s.innerType,t.io!=="input")return;let c=ls(s.defaultValue,e,t,r,n);if(c!==Pt)r._prefault=c};
var Ot=(e,t,r,n)=>{let s=e._zod.def;KC(s.innerType,t,n);let i=t.seen.get(e);i.ref=s.innerType;let c;try{c=s.catchValue(void 0)}catch{k(e,t,r,n,"Dynamic catch values are not supported in JSON Schema");return}r.default=c};
var Tt=(e,t,r,n)=>{let s=e._zod.def,i=s.in._zod.traits.has("$ZodTransform"),c=t.io==="input"?i?s.out:s.in:s.out;KC(c,t,n);let a=t.seen.get(e);a.ref=c};
var At=(e,t,r,n)=>{let s=e._zod.def;KC(s.innerType,t,n);let i=t.seen.get(e);i.ref=s.innerType,r.readOnly=!0};
var ge=(e,t,r,n)=>{let s=e._zod.def;KC(s.innerType,t,n);let i=t.seen.get(e);i.ref=s.innerType};
var fs=new WeakSet([Object.prototype,Error.prototype]);
function xe(e,t,r){Object.defineProperty(e,t,{configurable:!0,enumerable:!1,get(){let n=r(this);return Object.defineProperty(this,t,{value:n,configurable:!0,writable:!0}),n},set(n){Object.defineProperty(this,t,{value:n,configurable:!0,writable:!0})}})}
var ds=(e,t)=>{re.init(e,t),e.name="ZodError";let r=Object.getPrototypeOf(e);if(fs.has(r))return;fs.add(r),xe(r,"format",(n)=>(s)=>Nr(n,s)),xe(r,"flatten",(n)=>(s)=>Er(n,s)),xe(r,"addIssue",(n)=>(s)=>{n.issues.push(s),n.message=JSON.stringify(n.issues,sWn,2)}),xe(r,"addIssues",(n)=>(s)=>{n.issues.push(...s),n.message=JSON.stringify(n.issues,sWn,2)}),Object.defineProperty(r,"isEmpty",{configurable:!0,enumerable:!1,get(){return this.issues.length===0}})};
var S=dr("ZodError",ds,void 0,{Parent:Error});
var ms=se(S);
var hs=ie(S);
var jTr=F(S);
var _s=M(S);
var gs=Rr(S);
var xs=Dr(S);
var vs=jr(S);
var zs=Lr(S);
var ys=Fr(S);
var $s=Ur(S);
var bs=Mr(S);
var ks=Jr(S);
function zi(){if(!sZ.localeError)X8(rt())}
function Q(){if(!sZ.memoizer)X8({memoizer:Vn()})}
var DE=dr("ZodType",(e,t)=>(zi(),ME.init(e,t),e.def=t,e.type=t.type,e),{check(...e){let t=this.def;return this.clone(Xdt(t,{checks:[...t.checks??[],...e.map((r)=>typeof r==="function"?{_zod:{check:r,def:{check:"custom"},onattach:[]}}:r)]}),{parent:!0})},with(...e){return this.check(...e)},clone(e,t){return pUe(this,e,t)},brand(){return this},register(e,t){return e.add(this,t),this},refine(e,t){return this.check(cc(e,t))},superRefine(e,t){return this.check(uc(e,t))},overwrite(e){return this.check(E(e))},optional(){return Ai(this)},exactOptional(){return Gi(this)},nullable(){return Zs(this)},nullish(){return Ai(Zs(this))},nonoptional(e){return tc(this,e)},array(){return C(this)},or(e){return Be([this,e])},and(e){return ase(this,e)},transform(e){return ws(this,Es(e))},default(e){return Hi(this,e)},prefault(e){return ec(this,e)},catch(e){return rc(this,e)},pipe(e){return ws(this,e)},readonly(){return sc(this)},describe(e){let t=this.clone();return Q5e.add(t,{description:e}),t},meta(...e){if(e.length===0)return Q5e.get(this);let t=this.clone();return Q5e.add(t,e[0]),t},isOptional(){return this.safeParse(void 0).success},isNullable(){return this.safeParse(null).success},apply(e,...t){return t.length===0?e(this):e(this,...t)},get "~standard"(){return YCo(this,"~standard",{...Ae(this),jsonSchema:{input:K(this,"input"),output:K(this,"output")}})},set "~standard"(e){U0t(this,"~standard",e)},parse:function e(t,r){return ms(this,t,r,{callee:e})},parseAsync:async function e(t,r){return await hs(this,t,r,{callee:e})},safeParse(e,t){return jTr(this,e,t)},async safeParseAsync(e,t){return _s(this,e,t)},get spa(){return this?.safeParseAsync},set spa(e){U0t(this,"spa",e)},encode:function e(t,r){return gs(this,t,r,{callee:e})},decode:function e(t,r){return xs(this,t,r,{callee:e})},encodeAsync:async function e(t,r){return await vs(this,t,r,{callee:e})},decodeAsync:async function e(t,r){return await zs(this,t,r,{callee:e})},safeEncode(e,t){return ys(this,e,t)},safeDecode(e,t){return $s(this,e,t)},async safeEncodeAsync(e,t){return bs(this,e,t)},async safeDecodeAsync(e,t){return ks(this,e,t)},toJSONSchema(e){return Xn(this,{})(e)},get description(){return Q5e.get(this)?.description},get _def(){return this._zod.def}});
var Ps=dr("_ZodString",(e,t)=>{ae.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(n,s,i)=>it(e,n,s,i);let r=e._zod.bag;e.format=r.format??null,e.minLength=r.minimum??null,e.maxLength=r.maximum??null},{regex(...e){return this.check(Me(...e))},includes(...e){return this.check(Ke(...e))},startsWith(...e){return this.check(We(...e))},endsWith(...e){return this.check(Ge(...e))},min(...e){return this.check(R(...e))},max(...e){return this.check(Tnn(...e))},length(...e){return this.check(me(...e))},nonempty(...e){return this.check(R(1,...e))},lowercase(e){return this.check(Je(e))},uppercase(e){return this.check(Ve(e))},trim(){return this.check(qe())},normalize(...e){return this.check(Ye(...e))},toLowerCase(){return this.check(Xe())},toUpperCase(){return this.check(He())},slugify(){return this.check(Qe())}});
var Rnn=dr("ZodString",(e,t)=>{ae.init(e,t),Ps.init(e,t)},{email(e){return this.check(je(Ss,e))},url(e){return this.check(Fe(Is,e))},jwt(e){return this.check(Zn(ji,e))},emoji(e){return this.check(fn(ki,e))},guid(e){return this.check(un(bi,e))},uuid(e){return this.check(Le(X,e))},uuidv4(e){return this.check(an(X,e))},uuidv6(e){return this.check(ln(X,e))},uuidv7(e){return this.check(pn(X,e))},nanoid(e){return this.check(dn(Zi,e))},cuid(e){return this.check(mn(wi,e))},cuid2(e){return this.check(hn(Pi,e))},ulid(e){return this.check(_n(Si,e))},base64(e){return this.check(Ue(Os,e))},base64url(e){return this.check(bn(Ri,e))},xid(e){return this.check(gn(Ii,e))},ksuid(e){return this.check(xn(Oi,e))},ipv4(e){return this.check(vn(Ti,e))},ipv6(e){return this.check(zn(Ei,e))},cidrv4(e){return this.check(yn(Ni,e))},cidrv6(e){return this.check($n(Ci,e))},e164(e){return this.check(kn(Di,e))},datetime(e){return this.check(UTr(UCo,e))},date(e){return this.check(BTr(BCo,e))},time(e){return this.check(wn(yi,e))},duration(e){return this.check(Pn($i,e))}});
function o(e){return cn(Rnn,e)}
var b=dr("ZodStringFormat",(e,t)=>{z.init(e,t),Ps.init(e,t)});
var UCo=dr("ZodISODateTime",(e,t)=>{vo.init(e,t),b.init(e,t)});
var BCo=dr("ZodISODate",(e,t)=>{zo.init(e,t),b.init(e,t)});
var yi=dr("ZodISOTime",(e,t)=>{yo.init(e,t),b.init(e,t)});
var $i=dr("ZodISODuration",(e,t)=>{$o.init(e,t),b.init(e,t)});
var Ss=dr("ZodEmail",(e,t)=>{co.init(e,t),b.init(e,t)});
var bi=dr("ZodGUID",(e,t)=>{so.init(e,t),b.init(e,t)});
var X=dr("ZodUUID",(e,t)=>{io.init(e,t),b.init(e,t)});
var Is=dr("ZodURL",(e,t)=>{lo.init(e,t),b.init(e,t)});
var ki=dr("ZodEmoji",(e,t)=>{po.init(e,t),b.init(e,t)});
var Zi=dr("ZodNanoID",(e,t)=>{fo.init(e,t),b.init(e,t)});
var wi=dr("ZodCUID",(e,t)=>{mo.init(e,t),b.init(e,t)});
var Pi=dr("ZodCUID2",(e,t)=>{ho.init(e,t),b.init(e,t)});
var Si=dr("ZodULID",(e,t)=>{_o.init(e,t),b.init(e,t)});
var Ii=dr("ZodXID",(e,t)=>{go.init(e,t),b.init(e,t)});
var Oi=dr("ZodKSUID",(e,t)=>{xo.init(e,t),b.init(e,t)});
var Ti=dr("ZodIPv4",(e,t)=>{bo.init(e,t),b.init(e,t)});
var Ei=dr("ZodIPv6",(e,t)=>{Zo.init(e,t),b.init(e,t)});
var Ni=dr("ZodCIDRv4",(e,t)=>{wo.init(e,t),b.init(e,t)});
var Ci=dr("ZodCIDRv6",(e,t)=>{Po.init(e,t),b.init(e,t)});
var Os=dr("ZodBase64",(e,t)=>{Io.init(e,t),b.init(e,t)});
var Ri=dr("ZodBase64URL",(e,t)=>{Oo.init(e,t),b.init(e,t)});
var Di=dr("ZodE164",(e,t)=>{To.init(e,t),b.init(e,t)});
var ji=dr("ZodJWT",(e,t)=>{Ao.init(e,t),b.init(e,t)});
var D0t=dr("ZodNumber",(e,t)=>{Ee.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(n,s,i)=>ct(e,n,s,i);let r=e._zod.bag;e.minValue=Math.max(r.minimum??Number.NEGATIVE_INFINITY,r.exclusiveMinimum??Number.NEGATIVE_INFINITY)??null,e.maxValue=Math.min(r.maximum??Number.POSITIVE_INFINITY,r.exclusiveMaximum??Number.POSITIVE_INFINITY)??null,e.isInt=(r.format??"").includes("int")||Number.isSafeInteger(r.multipleOf??0.5),e.isFinite=!0,e.format=r.format??null},{gt(e,t){return this.check(fe(e,t))},gte(e,t){return this.check(B(e,t))},min(e,t){return this.check(B(e,t))},lt(e,t){return this.check(le(e,t))},lte(e,t){return this.check(J(e,t))},max(e,t){return this.check(J(e,t))},int(e){return this.check(mg(e))},safe(e){return this.check(mg(e))},positive(e){return this.check(fe(0,e))},nonnegative(e){return this.check(B(0,e))},negative(e){return this.check(le(0,e))},nonpositive(e){return this.check(J(0,e))},multipleOf(e,t){return this.check(de(e,t))},step(e,t){return this.check(de(e,t))},finite(){return this}});
function v(e){return Sn(D0t,e)}
var Ts=dr("ZodNumberFormat",(e,t)=>{Eo.init(e,t),D0t.init(e,t)});
function mg(e){return In(Ts,e)}
var xnn=dr("ZodBoolean",(e,t)=>{No.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(r,n,s)=>ut(e,r,n,s)});
function H(e){return Tn(xnn,e)}
var Li=dr("ZodUndefined",(e,t)=>{Co.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(r,n,s)=>lt(e,r,n,s)});
function EP(e){return An(Li,e)}
var Fi=dr("ZodNull",(e,t)=>{Ro.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(r,n,s)=>at(e,r,n,s)});
function Nm(e){return En(Fi,e)}
var Mi=dr("ZodUnknown",(e,t)=>{jo.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(r,n,s)=>dt(e,r,n,s)});
function oe(){return Cn(Mi)}
var Ji=dr("ZodNever",(e,t)=>{Lo.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(r,n,s)=>pt(e,r,n,s)});
function L0t(e){return Rn(Ji,e)}
var jCo=dr("ZodArray",(e,t)=>{Q(),Fo.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(r,n,s)=>vt(e,r,n,s),e.element=t.element},{min(e,t){return this.check(R(e,t))},nonempty(e){return this.check(R(1,e))},max(e,t){return this.check(Tnn(e,t))},length(e,t){return this.check(me(e,t))},unwrap(){return this.element}});
function C(e,t){return Dn(jCo,e,t)}
var vxe=dr("ZodObject",(e,t)=>{Q(),Jo.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(r,n,s)=>zt(e,r,n,s),L$s(e,"shape",(r)=>r._zod.def.shape,!1)},{keyof(){return U(Object.keys(this._zod.def.shape))},catchall(e){return this.clone({...this._zod.def,catchall:e})},passthrough(){return this.clone({...this._zod.def,catchall:oe()})},loose(){return this.clone({...this._zod.def,catchall:oe()})},strict(){return this.clone({...this._zod.def,catchall:L0t()})},strip(){return this.clone({...this._zod.def,catchall:void 0})},extend(e){return O$s(this,e)},safeExtend(e){return H$s(this,e)},merge(e){return M$s(this,e)},pick(e){return P$s(this,e)},omit(e){return I$s(this,e)},partial(...e){return RPs(rWn,this,e[0])},exactPartial(...e){return RPs(Ns,this,e[0],"exactPartial")},required(...e){return D$s(Cs,this,e[0])}});
function u(e,t){let r={type:"object",shape:e??{},...kl(t)};return new vxe(r)}
function _t(e,t){return new vxe({type:"object",shape:e,catchall:oe(),...kl(t)})}
var As=dr("ZodUnion",(e,t)=>{Ce.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(r,n,s)=>yt(e,r,n,s),e.options=t.options});
function Be(e,t){return new As({type:"union",options:e,...kl(t)})}
var Bi=dr("ZodDiscriminatedUnion",(e,t)=>{As.init(e,t),Bo.init(e,t)});
function Br(e,t,r){return new Bi({type:"union",options:t,discriminator:e,...kl(r)})}
var Vi=dr("ZodIntersection",(e,t)=>{Vo.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(r,n,s)=>$t(e,r,n,s)});
function ase(e,t){return new Vi({type:"intersection",left:e,right:t})}
var Ct=dr("ZodRecord",(e,t)=>{Q(),Wo.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(r,n,s)=>kt(e,r,n,s),e.keyType=t.keyType,e.valueType=t.valueType});
function pe(e,t,r){if(!t||!t._zod)return new Ct({type:"record",keyType:o(),valueType:e,...kl(t)});return new Ct({type:"record",keyType:e,valueType:t,...kl(r)})}
function Gue(e,t,r){return new Ct({type:"record",keyType:e,valueType:t,...kl(r),partial:!0})}
var nWn=dr("ZodEnum",(e,t)=>{Go.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(n,s,i)=>mt(e,n,s,i),e.enum=t.entries,e.options=Object.values(t.entries);let r=new Set(Object.keys(t.entries));e.extract=(n,s)=>{let i={};for(let c of n)if(r.has(c))i[c]=t.entries[c];else throw Error(`Key ${c} not found in enum`);return new nWn({...t,checks:[],...kl(s),entries:i})},e.exclude=(n,s)=>{let i={...t.entries};for(let c of n)if(r.has(c))delete i[c];else throw Error(`Key ${c} not found in enum`);return new nWn({...t,checks:[],...kl(s),entries:i})}});
function U(e,t){let r=Array.isArray(e)?Object.fromEntries(e.map((n)=>[n,n])):e;return new nWn({type:"enum",entries:r,...kl(t)})}
var WCo=dr("ZodLiteral",(e,t)=>{Yo.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(r,n,s)=>ht(e,r,n,s),e.values=new Set(t.values),Object.defineProperty(e,"value",{get(){if(t.values.length>1)throw Error("This schema contains multiple valid literal values. Use `.values` instead.");return t.values[0]}})});
function T(e,t){return new WCo({type:"literal",values:Array.isArray(e)?e:[e],...kl(t)})}
var Wi=dr("ZodTransform",(e,t)=>{Q(),qo.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(r,n,s)=>xt(e,r,n,s),e._zod.parse=(r,n)=>{if(n.direction==="backward")throw new aWn(e.constructor.name);r.addIssue=(i)=>{if(typeof i==="string")r.issues.push($0t(i,r.value,t));else{let c=i;if(c.fatal)c.continue=!1;if(c.code??(c.code="custom"),!("input"in c))c.input=r.value;c.inst??(c.inst=e),r.issues.push($0t(c))}};let s=t.transform(r.value,r);if(s instanceof Promise)return s.then((i)=>(r.value=i,r));return r.value=s,r}});
function Es(e){return new Wi({type:"transform",transform:e})}
var rWn=dr("ZodOptional",(e,t)=>{Re.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(r,n,s)=>ge(e,r,n,s),e.unwrap=()=>e._zod.def.innerType});
function Ai(e){return new rWn({type:"optional",innerType:e})}
var Ns=dr("ZodExactOptional",(e,t)=>{Xo.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(r,n,s)=>ge(e,r,n,s),e.unwrap=()=>e._zod.def.innerType});
function Gi(e){return new Ns({type:"optional",innerType:e})}
var qi=dr("ZodNullable",(e,t)=>{Ho.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(r,n,s)=>Zt(e,r,n,s),e.unwrap=()=>e._zod.def.innerType});
function Zs(e){return new qi({type:"nullable",innerType:e})}
var Xi=dr("ZodDefault",(e,t)=>{Qo.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(r,n,s)=>St(e,r,n,s),e.unwrap=()=>e._zod.def.innerType,e.removeDefault=e.unwrap});
function Hi(e,t){return new Xi({type:"default",innerType:e,get defaultValue(){return typeof t==="function"?t():VCo(t)}})}
var Qi=dr("ZodPrefault",(e,t)=>{en.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(r,n,s)=>It(e,r,n,s),e.unwrap=()=>e._zod.def.innerType});
function ec(e,t){return new Qi({type:"prefault",innerType:e,get defaultValue(){return typeof t==="function"?t():VCo(t)}})}
var Cs=dr("ZodNonOptional",(e,t)=>{tn.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(r,n,s)=>wt(e,r,n,s),e.unwrap=()=>e._zod.def.innerType});
function tc(e,t){return new Cs({type:"nonoptional",innerType:e,...kl(t)})}
var WTr=dr("ZodCatch",(e,t)=>{rn.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(r,n,s)=>Ot(e,r,n,s),e.unwrap=()=>e._zod.def.innerType,e.removeCatch=e.unwrap});
function rc(e,t){return new WTr({type:"catch",innerType:e,catchValue:typeof t==="function"?t:IPs(t)})}
var oWn=dr("ZodPipe",(e,t)=>{De.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(r,n,s)=>Tt(e,r,n,s),e.in=t.in,e.out=t.out});
function ws(e,t){return new oWn({type:"pipe",in:e,out:t})}
var oc=dr("ZodPreprocess",(e,t)=>{oWn.init(e,t),on.init(e,t)});
var nc=dr("ZodReadonly",(e,t)=>{nn.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(r,n,s)=>At(e,r,n,s),e.unwrap=()=>e._zod.def.innerType});
function sc(e){return new nc({type:"readonly",innerType:e})}
var Rs=dr("ZodCustom",(e,t)=>{sn.init(e,t),DE.init(e,t),e._zod.processJSONSchema=(r,n,s)=>gt(e,r,n,s)});
function Zd(e,t){return jn(Rs,e??(()=>!0),t)}
function cc(e,t={}){return Ln(Rs,e,t)}
function uc(e,t){return Fn(e,t)}
function Yi(e,t){return new oc({type:"pipe",in:Es(e),out:t})}
export { u as u };
export { o as o };
export { v as v };
export { H as H };
export { C as C };
export { Be as Be };
export { U as U };
export { T as T };
export { Gue as Gue };
export { oe as oe };
export { _t as _t };
export { Yi as Yi };
export { pe as pe };
export { Br as Br };
export { ME as ME };
export { DE as DE };
export { KC as KC };
export { EP as EP };
export { WTr as WTr };
export { Nm as Nm };
export { Zd as Zd };
export { rWn as rWn };
export { oWn as oWn };
export { vxe as vxe };
export { fPs as fPs };
export { Rnn as Rnn };
