// Extracted source closure for 2.1.295; source module src_188478200.js sha256 ead1262758c21faf943e5b1cc20528b94fba5f9c0299d1987e6a3a41665abc25
import { Ge } from "./src_186068928.mjs";
import { pTs } from "./src_188476195.mjs";
import { O } from "./src_186477637.mjs";
import { hostname as o } from "os";
class e{pidSpace=null;pidDomain=void 0;uidsCollapse=null}
var f$e=Ge(new e,(n)=>{n.pidDomain=void 0});
function sue(){if(f$e.pidSpace===null){let n="";f$e.pidSpace=`${o()}${n===""?"":"#"+n}`}return f$e.pidSpace}
function WC(){return f$e.pidDomain??=(async()=>pTs(O()))().catch((n)=>{throw f$e.pidDomain=void 0,n}),f$e.pidDomain}
export { WC as WC };
export { sue as sue };
