// Extracted source closure for 2.1.295; source module src_190631345.js sha256 38f48e568ed3ab362b009bb80a9016a78a72206feb1059890125382948c26669
import { q, B, De, we } from "./src_186096608.mjs";
import { ec, Ixe } from "./src_187649719.mjs";
import { ie } from "./src_186240709.mjs";
import { sep as tn, join as Bi } from "path";
import { vO } from "./src_188118649.mjs";
class Jo{claudeTempDir=void 0;childProcessTmpDir=void 0;bundledSkillsRoot=void 0;scratchpadDirBySessionId=new Map;resolvedWorkingDirPaths=new Map;compiledPatternsByRules=new WeakMap;physicalTwinsByPattern=new Map;giveUpProbes=new WeakMap;reportedGiveUps=new Set;trustedSymlinkEquivalences=void 0;userSkillsBaseSpellingsFolded=void 0;resolvedSandboxConfigPaths=new Map;canonicalHomedirByHome=new Map;foldedSegmentsBySpelling=new Map;settingsFileLinks=void 0;reset(){this.claudeTempDir=void 0,this.childProcessTmpDir=void 0,this.bundledSkillsRoot=void 0,this.scratchpadDirBySessionId.clear(),this.resolvedWorkingDirPaths.clear(),this.trustedSymlinkEquivalences=void 0,this.userSkillsBaseSpellingsFolded=void 0,this.resolvedSandboxConfigPaths.clear(),this.canonicalHomedirByHome.clear(),this.foldedSegmentsBySpelling.clear(),this.physicalTwinsByPattern.clear(),this.reportedGiveUps.clear(),this.settingsFileLinks=void 0}}
var Li=new q(()=>new Jo);
function zF(){return Li.of(B().host)}
function _f(){let e=zF();if(e.claudeTempDir!==void 0&&!De())return e.claudeTempDir;let n=ec(),s=ie(),r=n;try{r=s.realpathSync(n)}catch{}return e.claudeTempDir=r+tn,e.claudeTempDir}
function YAe(){let e=zF();if(e.childProcessTmpDir!==void 0&&!De())return e.childProcessTmpDir;let n=Ixe(),s=ie(),r=n;try{r=s.realpathSync(n)}catch{}return e.childProcessTmpDir=r+tn,e.childProcessTmpDir}
function $x(){return FYt(we())}
function FYt(e){return Bi(_f(),vO(e))+tn}
export { _f as _f };
export { YAe as YAe };
export { $x as $x };
