// Extracted source closure for 2.1.295; source module src_188305035.js sha256 7d61a8711afadefeb749f07516d762956e786edcaa436ea0a176a41b358017a0
function v1e(){return!1}
var i=/^(?:--port(?:=\d+)?|\d+|--result-format(?:=(?:raw|rendered))?|raw|rendered|--session-tunnel|--debug|-d|--verbose)$/;
function nu(){{let[n,o,r,...e]=process.argv.slice(2),t=r==="--transport=http"?e:r==="--transport"&&e[0]==="http"?e.slice(1):void 0;return v1e()&&n==="mcp"&&o==="serve"&&t!==void 0&&t.every((s)=>i.test(s))}return!1}
export { nu as nu };
