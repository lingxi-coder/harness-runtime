// Extracted source closure for 2.1.295; source module src_188476195.js sha256 75ab3f7ca9487cd77e0bb14cb64615f3bb5c4d674680e33310e68fb52a8d61f7
import { hostname as i } from "os";
import { readFile as r, readlink as s } from "fs/promises";
async function pTs(n){if(n==="windows")return`darwin:${i().toLowerCase()}`;if(n!=="linux"&&n!=="wsl")return"darwin";let[e,t]=await Promise.all([r("/etc/machine-id","utf8").then((o)=>o.trim(),()=>""),s("/proc/self/ns/pid").catch(()=>"")]);return`darwin:${e}:${t}`}
export { pTs as pTs };
