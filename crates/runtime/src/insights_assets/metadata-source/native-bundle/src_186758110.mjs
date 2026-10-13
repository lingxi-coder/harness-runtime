// Extracted source closure for 2.1.295; source module src_186758110.js sha256 17b497698fa3c5773f53f421e45c833b5dc36f62a5e6345ec9105685701a18e4
var o=new Set(["localhost","::1","[::1]"]);
var n=/^127(?:\.(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)){3}$/;
function qk(t){return o.has(t)||n.test(t)}
export { qk as qk };
