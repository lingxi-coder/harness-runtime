// Extracted source closure for 2.1.295; source module src_186669406.js sha256 c3942261ed3b30a8e1da9dd48bc610e7cefb78cb4abf14f4b8419a1668a755b1
import { So, Lon, Non, pt, B } from "./src_186096608.mjs";
import { a } from "./src_186481843.mjs";
import { Ie } from "./src_186092222.mjs";
function Ovo(){return{validatedModels:new Set,confirmedEntitlementDenies:new Map,overturnedEntitlementDenies:new Map,bedrockInferenceProfiles:void 0,inferenceProfileBackingModels:new Map,modelConfigs:new Map,clientDataFetches:new Map,capabilityOverrides:new Map,probeResults:new Map,preconnectFired:!1}}
function Yjn(){return{allModelBetas:new Map,modelBetas:new Map,bedrockExtraBodyParamsBetas:new Map,midConversationSystem:new Map}}
function G(){return{providerCache:Ovo(),fetchesClientData:!1,sessionModelDenyReprobe:{reprobe:void 0,honoredModels:new Set,contestedDenies:new Map,refusedModels:new Set,generation:0,restricted:Ie()},betas:Yjn(),betasCompiledOnly:Yjn(),usageCreditsOnlyModelFlags:{fableBilling:null,creditsOnlyTiers:null},warnedEnforcementMessages:new Set,resolvingBestModel:!1,modelCapabilitiesByCachePath:new Map,gatewayModelsByCachePath:new Map,modelCapabilitiesStorageV5:void 0,gatewayModelsStorageV5:void 0,servedCatalogByKey:new Map,servedCatalogRefreshes:new Map,servedCatalogSettledRefreshes:new WeakSet,servedCatalogRefusal:void 0,servedCatalogPostLoginRun:void 0,publishedCatalogByKey:new Map,publishedCatalogRefreshes:new Map,publishedCatalogFloorMarks:new Map,publishedCatalogFloorRead:void 0,publishedCatalogFloorWrite:void 0,clientDataUrl:{pending:void 0,document:void 0,assumesFirstPartyBaseUrl:!1},servedCatalogShadowStarted:!1,servedCatalogActive:void 0,servedCatalogMaskedIds:new Set,servedCatalogMasksEveryId:!1,servedCatalogPublicIds:new Map,servedCatalogMaskHydrated:!1,servedCatalogMaskHydration:void 0,servedCatalogMaskRetryAt:0,servedCatalogPersistedMaskedIds:void 0,servedCatalogMaskWrite:void 0,servedCatalogSuppressDepth:0,servedCatalogDeactivatedRows:void 0,familySpellingVerdicts:new Map,behavesAsMemo:void 0,unservedModelDrop:void 0,servedCatalogOffBeforeAsking:void 0,catalogDecision:void 0}}
var F=new pt(()=>G());
function Dr(){return F.of(B())}
function Pe(){if(So()||Lon()||Non())return"gateway";return a.CLAUDE_CODE_USE_BEDROCK?"bedrock":a.CLAUDE_CODE_USE_FOUNDRY?"foundry":a.CLAUDE_CODE_USE_ANTHROPIC_AWS?"anthropicAws":a.CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD?"anthropicGoogleCloud":a.CLAUDE_CODE_USE_MANTLE?"mantle":a.CLAUDE_CODE_USE_VERTEX?"vertex":"firstParty"}
function qn(){return Pe()==="firstParty"}
function tf(){let e=process.env.ANTHROPIC_BASE_URL;if(!e)return!0;return nh(e)}
function nh(e){try{let t=new URL(e).host;return["api.anthropic.com"].includes(t)}catch{return!1}}
export { qn as qn };
export { Pe as Pe };
export { tf as tf };
export { nh as nh };
export { Dr as Dr };
export { Yjn as Yjn };
