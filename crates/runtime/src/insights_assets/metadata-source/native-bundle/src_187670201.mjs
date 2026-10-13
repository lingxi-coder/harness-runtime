// Extracted source closure for 2.1.295; source module src_187670201.js sha256 76dcd4b3d2da83a9609a70d4b76ba232541f96cb7c1caf15a90d4b5bccaf21cc
import { s1e, Mxr, jbe, Nxr, XHt, Lxr, KHt, YHt, Fxr, rj, i8e, ype, Pse, r8e, PZ } from "./src_186096608.mjs";
import { eC, ewe } from "./src_186068928.mjs";
var v=Object.prototype;
var d=v.hasOwnProperty;
function g(r,t,e){var o=r[t];if(!(d.call(r,t)&&ewe(o,e))||e===void 0&&!(t in r))PZ(r,t,e)}
var v9e=g;
function P(r,t,e,o){if(!eC(r))return r;t=ype(t,r);var i=-1,m=t.length,s=m-1,n=r;while(n!=null&&++i<m){var f=Pse(t[i]),a=e;if(f==="__proto__"||f==="constructor"||f==="prototype")return r;if(i!=s){var p=n[f];if(a=o?o(p,f,n):void 0,a===void 0)a=eC(p)?p:r8e(t[i+1])?[]:{}}v9e(n,f,a),n=n[f]}return r}
var u=P;
function x(r,t,e){var o=-1,i=t.length,m={};while(++o<i){var s=t[o],n=i8e(r,s);if(e(n,s))u(m,ype(s,r),n)}return m}
var ORr=x;
var O=Fxr(Object.getPrototypeOf,Object);
var hrn=O;
var I=Object.getOwnPropertySymbols;
var h=!I?Lxr:function(r){var t=[];while(r)KHt(t,YHt(r)),r=hrn(r);return t};
var HRr=h;
function K(r){var t=[];if(r!=null)for(var e in Object(r))t.push(e);return t}
var l=K;
var c=Object.prototype;
var A=c.hasOwnProperty;
function S(r){if(!eC(r))return l(r);var t=XHt(r),e=[];for(var o in r)if(!(o=="constructor"&&(t||!A.call(r,o))))e.push(o);return e}
var y=S;
function w(r){return jbe(r)?Nxr(r,!0):y(r)}
var OUe=w;
function b(r){return Mxr(r,OUe,HRr)}
var yrn=b;
function _(r,t){if(r==null)return{};var e=s1e(yrn(r),function(o){return[o]});return t=rj(t),ORr(r,e,function(o,i){return t(o,i[0])})}
var fo=_;
export { fo as fo };
export { v9e as v9e };
export { yrn as yrn };
export { hrn as hrn };
export { HRr as HRr };
export { OUe as OUe };
export { ORr as ORr };
