// Extracted source closure for 2.1.295; source module src_186563900.js sha256 ebeb9fd50efa585aae42d0d35761cfffd96b82a96192e02513fad430778785af
import { kse } from "./src_186481843.mjs";
var Cue=["claude-3-5-haiku","claude-3-5-sonnet","claude-3-7-sonnet","claude-fable-5","claude-fable-5-1","claude-haiku-4-5","claude-haiku-5-5","claude-mythos-5","claude-mythos-5-1","claude-opus-4-0","claude-opus-4-1","claude-opus-4-5","claude-opus-4-6","claude-opus-4-7","claude-opus-4-8","claude-opus-5","claude-opus-5-5","claude-sonnet-4-0","claude-sonnet-4-5","claude-sonnet-4-6","claude-sonnet-5","claude-sonnet-5-5"];
var tV=["sonnet","opus","haiku","fable","best","sonnet[1m]","opus[1m]","fable[1m]","opusplan"];
var nV=["sonnet","opus","haiku","fable"];
function ty(e){return tV.includes(e)}
function Ot(e){return e.replace(/\[1m\]$/i,"")}
function Fn(e){return e.replace(/\[(1|2)m\]/gi,"")}
function ny(e){return nV.includes(e)}
function wp(e){let n=e.trim().toLowerCase();if(n===""||/\s/.test(n))return null;n=n.replace(/\[[12]m\]$/,"");let s=n.lastIndexOf("/");if(s!==-1)n=n.slice(s+1);let t=/^(?:([a-z-]+)\.)?anthropic\.(claude-.*)$/.exec(n);if(t){let[,a,l=""]=t;if(a!==void 0&&!kse.includes(a))return null;n=l}let r=u(n);if(!r)return null;let o=n.slice(r.base.length);if(o!==""&&!/^[-@]/.test(o))return null;let i={family:r.family,major:r.major,legacyVersionFirst:r.legacyVersionFirst,base:r.base};if(r.minor!==void 0)i.minor=r.minor;if(!Mvo(o))i.trailer=o;else{let a=/(?:-v\d+@|[-@])(\d{8})/.exec(o)?.[1];if(a!==void 0)i.date=a}return i}
function Mvo(e){return/^(?:-fast|-latest)?(?:-v\d{1,3}@\d{8}|[-@]\d{8})?(?:-v\d{1,3}(?::\d{1,3})?)?$/.test(e)}
function $$e(e,n){return e.major-n.major||(e.minor??0)-(n.minor??0)}
function u(e){let n=/^claude-([a-z]+)-(\d{1,2})(?!\d)(?:-(\d{1,2})(?!\d))?/.exec(e);if(n){let[t,r="",o="",i]=n;return{family:r,major:Number(o),minor:i===void 0?void 0:Number(i),legacyVersionFirst:!1,base:t}}let s=/^claude-(\d{1,2})(?!\d)(?:-(\d{1,2})(?!\d))?-([a-z]+)/.exec(e);if(s){let[t,r="",o,i=""]=s;return{family:i,major:Number(r),minor:o===void 0?void 0:Number(o),legacyVersionFirst:!0,base:t}}return null}
export { Fn as Fn };
export { ny as ny };
export { ty as ty };
export { wp as wp };
export { nV as nV };
export { Ot as Ot };
export { Cue as Cue };
export { $$e as $$e };
