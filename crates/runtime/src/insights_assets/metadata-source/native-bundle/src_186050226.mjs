// Extracted source closure for 2.1.295; source module src_186050226.js sha256 d849efeac1fe84217d5882f3b1b6af63cfc9437437d20ea8a9070232c89e0c7c
var l=Object.create;
var {getPrototypeOf:m,defineProperty:g,getOwnPropertyNames:n}=Object;
var k=Object.prototype.hasOwnProperty;
function o(a){return this[a]}
var p;
var q;
var be=(a,c,d)=>{var e=a!=null&&typeof a==="object";if(e){var i=c?p??=new WeakMap:q??=new WeakMap,j=i.get(a);if(j)return j}d=a!=null?l(m(a)):{};let f=c||!a||!a.__esModule||!k.call(a,"default")?g(d,"default",{value:a,enumerable:!0}):d;if(a&&typeof a==="object"||typeof a==="function"){for(let h of n(a))if(!k.call(f,h))g(f,h,{get:o.bind(a,h),enumerable:!0})}if(e)i.set(a,f);return f};
var A=(a,c)=>()=>(c||a((c={exports:{}}).exports,c),c.exports);
var Me=import.meta.require;
export { be as be };
export { A as A };
export { Me as Me };
