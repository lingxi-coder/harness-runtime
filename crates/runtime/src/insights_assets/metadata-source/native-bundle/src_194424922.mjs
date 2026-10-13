// Extracted source closure for 2.1.295; source module src_194424922.js sha256 500074b393e24d7202ae413c2859a7fa9a0c2d1e552311dff7cc1ebe07061d41
import { ml } from "./src_186231195.mjs";
function Es(n,g){if(!n.trim()||!g.trim())return null;let c=ml(g),l=new RegExp(`<${c}(?=[\\s>])`,"gi"),e=new RegExp(`<\\/${c}>`,"gi"),s=0;for(;;){l.lastIndex=s;let t=l.exec(n);e.lastIndex=s;let o=e.exec(n);if(!t||!o)return null;let r=n.indexOf(">",t.index+t[0].length);if(r===-1)return null;e.lastIndex=r+1;let i=e.exec(n);if(!i)return null;let a=o.index+o[0].length<=t.index,x=n.slice(r+1,i.index);if(x&&!a)return x;s=i.index+i[0].length}}
export { Es as Es };
