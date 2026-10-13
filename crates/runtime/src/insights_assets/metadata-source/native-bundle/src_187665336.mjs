// Extracted source closure for 2.1.295; source module src_187665336.js sha256 e0e546c59e476b49cf15faafbcbabf47cbfc2f155de2e34a22e549a7d4bed888
import { userInfo as s } from "os";
var t="com.anthropic.claudecode";
var Vjn="HKLM\\SOFTWARE\\Policies\\ClaudeCode";
var qjn="HKCU\\SOFTWARE\\Policies\\ClaudeCode";
var dtn="Settings";
var Avo="/usr/bin/plutil";
var KRs=["-convert","json","-o","-","--"];
var YRs=["-lint","-s","--"];
var XRs=5000;
var _Ar=2097152;
var JRs="/mnt/c/Windows/System32/reg.exe";
var lH="/mnt/c/Program Files/ClaudeCode";
function KOt(){return!1}
function QRs(){let e="";try{e=s().username}catch{}let r=[];if(e)r.push({path:`/Library/Managed Preferences/${e}/${t}.plist`,label:"per-user managed preferences"});return r.push({path:`/Library/Managed Preferences/${t}.plist`,label:"device-level managed preferences"}),r}
export { lH as lH };
export { Vjn as Vjn };
export { dtn as dtn };
export { KOt as KOt };
export { qjn as qjn };
export { QRs as QRs };
export { XRs as XRs };
export { Avo as Avo };
export { KRs as KRs };
export { _Ar as _Ar };
export { YRs as YRs };
export { JRs as JRs };
