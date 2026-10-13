// Extracted source closure for 2.1.295; source module src_188442564.js sha256 857d9b766d74a7f5bf26b64e812924eef31b07a6abf457e7ece16f6c4c48b7da
import { join as u } from "path";
import { homedir as o, userInfo as l } from "os";
import { Ee } from "./src_186220896.mjs";
import { createHash as i } from "crypto";
import { cn } from "./src_186448966.mjs";
import { q, B } from "./src_186096608.mjs";
var nbe="-credentials";
function Pw(){let e=process.env.CLAUDE_SECURESTORAGE_CONFIG_DIR;if(e!==void 0)return(e||u(o(),".claude")).normalize("NFC");return Ee()}
function O$(e=""){let n=process.env.CLAUDE_SECURESTORAGE_CONFIG_DIR,t=n!==void 0?!n:!process.env.CLAUDE_CONFIG_DIR,r=n!==void 0?n.normalize("NFC"):Ee(),c=t?"":`-${i("sha256").update(r).digest("hex").substring(0,8)}`;return`Claude Code${cn().OAUTH_FILE_SUFFIX}${e}${c}`}
var s=/^[a-zA-Z0-9._-]+$/;
function vR(){let e;try{e=process.env.USER||l().username}catch{e="claude-code-user"}if(!s.test(e))return"claude-code-user";return e}
var UOt=30000;
class a{cache={data:null,cachedAt:0};generation=0;readInFlight=null;lastReadFailure=null;keychainHoldsItem=!1;lastKnown=null;legacyApiKeyPrefetch=null}
var d=new q(()=>new a);
function O3(){return d.of(B().host)}
var gEo=1000;
function iH(){let e=O3();e.cache={data:null,cachedAt:0},e.generation++,e.readInFlight=null,e.lastReadFailure=null}
function Wen(e,n,t){if(e.generation===t)e.keychainHoldsItem=n}
export { Pw as Pw };
export { O$ as O$ };
export { vR as vR };
export { iH as iH };
export { UOt as UOt };
export { O3 as O3 };
export { gEo as gEo };
export { nbe as nbe };
export { Wen as Wen };
