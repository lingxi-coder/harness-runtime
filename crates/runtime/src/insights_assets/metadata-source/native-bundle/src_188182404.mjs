// Extracted source closure for 2.1.295; source module src_188182404.js sha256 a069f193bd5135e43d946f08eeee90bd631a3cd88c962c5c71ed70a216de8c36
var d=/^[a-zA-Z0-9_-]+$/;
function KB(e){return e!==""&&d.test(e)}
function ir(e){return e.replace(/^(?:session|cse)_/,"")}
function _V(e){return(e.startsWith("session_")||e.startsWith("cse_"))&&KB(e)&&ir(e)!==""}
export { _V as _V };
