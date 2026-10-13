// Extracted source closure for 2.1.295; source module src_186925971.js sha256 8a2402fdfbb0de155abc8a310bd5c71ff8c5d99a36990a3e5e265023bc6b7202
import { lstat as F, readFile as U } from "fs/promises";
async function Fa(e,n){try{let r=await F(e);if(!r.isFile()||r.size>n)return null;return await U(e,"utf8")}catch{return null}}
export { Fa as Fa };
