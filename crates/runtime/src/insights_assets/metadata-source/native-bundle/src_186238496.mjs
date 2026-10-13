// Extracted source closure for 2.1.295; source module src_186238496.js sha256 d779a37559646b6275df317a91e586ed1fe2c661e476e55707300bd388537e6e
function ja(e,t){return{code:"InvalidArgument",argument:e,...t!==void 0&&{reason:t}}}
var Rxr="Unsupported";
function Dut(e){return e8e(e)&&e.code==="Failed"&&e.telemetryCode===Rxr}
var n="ByteViewUnsupported";
function WHt(e){return e.code==="Failed"&&"telemetryCode"in e&&e.telemetryCode===n}
var o=new Set(["InvalidArgument","NotFound","AlreadyExists","PreconditionFailed","LeaseHeld","Unavailable","Failed","ScopeNotFound"]);
function e8e(e){return typeof e==="object"&&e!==null&&"code"in e&&typeof e.code==="string"&&o.has(e.code)}
function gt(e){return e.code+("failureClass"in e?` ${e.failureClass}`:"")+("telemetryCode"in e&&e.telemetryCode?` ${e.telemetryCode}`:"")+("cause"in e&&e.cause?`: ${i(e.cause)}`:"")}
function i(e){return e instanceof Error?e.message:String(e)}
export { WHt as WHt };
export { ja as ja };
export { gt as gt };
export { Rxr as Rxr };
export { Dut as Dut };
