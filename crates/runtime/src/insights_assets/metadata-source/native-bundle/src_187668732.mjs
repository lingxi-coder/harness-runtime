// Extracted source closure for 2.1.295; source module src_187668732.js sha256 d98858236e24c5f6eec78791a0083edeea0d1c0767a7e4af0578942f14cc4dc6
import { Z$ } from "./src_186240709.mjs";
function $nn(r){let t=/^([^@:/[\]]+)@([^@:/[\]]+):(.*)$/s.exec(r);return t&&!Z$(r)?{user:t[1],host:t[2],path:t[3]}:null}
export { $nn as $nn };
