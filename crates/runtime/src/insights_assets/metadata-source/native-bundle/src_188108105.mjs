// Extracted source closure for 2.1.295; source module src_188108105.js sha256 aad30931bcf5032a1dde4c7c87fc66513c41971f29bcf387d15313c3e93c5f02
import { AsyncLocalStorage as l } from "async_hooks";
import { UR, we, jn, IO, ql } from "./src_186096608.mjs";
function br(){return IO().remote}
function Mt(){return jn()||br()!==null}
var t=new l;
function SZ(){return t.getStore()!==void 0}
function Irn(){return t.getStore()?.cwd??UR()}
function fm(){if(t.getStore()?.cwd===void 0&&ql()===null)return null;return se()}
function se(){try{return Irn()}catch{return we()}}
function ZRr(){return!SZ()&&Mt()?void 0:se()}
export { se as se };
export { Mt as Mt };
export { fm as fm };
export { ZRr as ZRr };
