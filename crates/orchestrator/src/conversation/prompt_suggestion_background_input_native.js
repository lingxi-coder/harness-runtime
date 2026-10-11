const backgroundInput = (() => {
const ht = new Set(["ScheduleWakeup", "CronCreate", "Monitor"]);
function TF(e){if(e.type!=="assistant")return null;if(Array.isArray(e.message.content))return e.message.content.map((n)=>{if(n.type==="text")return n.text;return""}).filter((n)=>n!=="").join(`
`).trim()||null;return null}
function he(e){let n=new Map;for(let r of e)if(Array.isArray(r.message.content)){for(let u of r.message.content)if(u.type==="tool_use"&&!ht.has(u.name))n.set(u.name,(n.get(u.name)??0)+1)}return[...n].sort((r,u)=>u[1]-r[1]).slice(0,5).map(([r,u])=>u>1?`${r}\xD7${u}`:r).join(", ")}
return {text: TF, tools: he};
})();
