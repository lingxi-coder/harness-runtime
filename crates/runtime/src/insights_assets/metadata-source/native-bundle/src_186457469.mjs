// Extracted source closure for 2.1.295; source module src_186457469.js sha256 db81ee2e6c663910576b8de9592b96964caa9d687d4b506129667fe8fc12d4d6
function Cse(t){let e=0;for(let n=0;n<t.length;n++)e=(e<<5)-e+t.charCodeAt(n)|0;return e}
function Grn(t){return`</pasted_content id="${t}">
`}
function aRo(t){if(t.length!==4)return!1;for(let e=0;e<t.length;e++){let n=t.charCodeAt(e);if(!(n>=48&&n<=57||n>=97&&n<=102))return!1}return!0}
function UHt(t){let e=[],n=0,s=0;for(;;){let c=t.indexOf('<pasted_content id="',s);if(c===-1)break;let r=c+20,o=t.slice(r,r+4);if(!aRo(o)||!t.startsWith(`">
`,r+4)){s=r;continue}let i=r+4+3,f=Grn(o).slice(0,-1),l=t.indexOf(`
${f}`,i-1)+1;if(l===0)break;let E=c;for(let d=0;d<2&&E>n&&t[E-1]===`
`;d++)E--;if(E>n)e.push({kind:"text",text:t.slice(n,E)});n=l+f.length;for(let d=0;d<2&&t[n]===`
`;d++)n++;e.push({kind:"block",id:o,body:t.slice(i,l-1),raw:t.slice(E,n)}),s=n}if(n<t.length)e.push({kind:"text",text:t.slice(n)});return e}
function Lbe(t){let e=UHt(t);if(e.length===1&&e[0].kind==="text")return t;return e.map((n)=>n.kind==="text"?n.text:n.body).join("")}
export { Lbe as Lbe };
export { Cse as Cse };
