// Extracted source closure for 2.1.295; source module src_187672238.js sha256 f3ed45fef5ea171f0cfa0f67af87b7a9393fa92c3eb2c67ef806a92587319a4c
import { createHash as r } from "crypto";
function ct(e){if(typeof e!=="string"&&!ArrayBuffer.isView(e))throw Object.assign(TypeError("sha256Hex: the data is neither text nor bytes"),{code:"ERR_INVALID_ARG_TYPE"});return r("sha256").update(e).digest("hex")}
export { ct as ct };
