// Extracted source closure for 2.1.295; source module src_188602688.js sha256 2f5ed52f931f4c8d348412bc92110c17e45d5f60805ef9228ba6a62e67bc4604
import { a } from "./src_186481843.mjs";
import { qn } from "./src_186669406.mjs";
import { So } from "./src_186096608.mjs";
import { J$ } from "./src_186460102.mjs";
function u(){if(a.CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST)return!1;return!qn()}
function l(){return a.CLAUDE_CODE_CUSTOM_OAUTH_URL!==void 0}
function Hy(){return u()||So()!==null||J$()||l()}
var Hen=/[\u200c-\u200f\u202a-\u202e\u206a-\u206f\ufeff]/;
var Qwo=new RegExp(Hen,"g");
export { Qwo as Qwo };
export { Hy as Hy };
