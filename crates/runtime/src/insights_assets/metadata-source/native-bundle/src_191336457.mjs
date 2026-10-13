// Extracted source closure for 2.1.295; source module src_191336457.js sha256 83fefb12449aee7d1bf6cb238c90bff4caca18ab8e0a402ee2185a2af39e389d
import { PS, zn } from "./src_188693222.mjs";
import { join as b } from "path";
import { K, De } from "./src_186096608.mjs";
import { $x } from "./src_190631345.mjs";
function BAt(){let e=PS()?.adoptShellOutputRoot;if(e!==void 0)return b(e,K(),"tasks");let n=zn();if(n.outputDir===void 0||De())n.outputDir=b($x(),K(),"tasks");return n.outputDir}
function cco(){return PS()?.adoptShellOutputRoot!==void 0?BAt():(De()?void 0:zn().outputDir)??b($x(),K(),"tasks")}
export { cco as cco };
