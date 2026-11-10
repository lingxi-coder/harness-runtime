// Immutable MCP OAuth provider-state helpers, fake credential store only.
// No real account, keychain, network, browser or installed-CLI state access.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync,mkdirSync} from 'node:fs';
import {homedir} from 'node:os';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import vm from 'node:vm';

const SHA='a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450';
const hash=value=>createHash('sha256').update(value).digest('hex');
const json=value=>JSON.stringify(value);
const clone=value=>JSON.parse(json(value));

export async function generateOAuthStateFixture(binaryPath=path.join(homedir(),'.local/share/claude/versions/2.1.322'),artifactPath) {
 const binary=readFileSync(binaryPath);assert.equal(hash(binary),SHA);
 const ranges=[],sources={};
 const slice=(name,start,end)=>{assert(start>=0&&end>start);const bytes=binary.subarray(start,end);ranges.push({name,start,end,sha256:hash(bytes)});return sources[name]=bytes.toString('utf8')};
 const extract=(name,start,end,from=305162178)=>{const a=binary.indexOf(start,from),z=binary.indexOf(end,a);return slice(name,a,z)};
 const resolve=extract('resolve-client','async resolveClientInformation(){','async patchStoredClientEntry(');
 const patch=extract('patch-client','async patchStoredClientEntry(','async saveClientInformation(');
 const saveClient=extract('save-client','async saveClientInformation(','async tokens(){');
 const saveTokens=extract('save-tokens','async saveTokens(','logTokenPersistFailed(e,t){');
 const invalidate=extract('invalidate-credentials','async invalidateCredentials(','async saveDiscoveryState(');
 const saveDiscovery=extract('save-discovery','async saveDiscoveryState(','servePolicyCheckedDiscoveryState(');
 const served=extract('served-metadata-url','servedMetadataUrl(){','async discoveryState(){');
 const clientMetadataUrl=extract('client-metadata-url','get clientMetadataUrl(){','setMetadata(');
 const ports=extract('callback-port-and-cimd','function Te(','function XAa(');
 const binderStart=binary.indexOf('function w(e){try{let t=new URL(e)',270334219);
 const binders=slice('metadata-state-binders',binderStart,binary.indexOf('function Vmo(',binderStart));
 const cache=extract('token-cache-mutators','function LTn(','function HTn(',305150000);
 const recent=extract('tokenless-rejection-window','var H=604800000,w=60000;','function Ydr(',305150000);
 const oc=extract('sdk-issuer-filter','function oc(','function _c(',305000000);
 const scope=extract('sdk-scope','function Gd(','async function En(',305000000);
 const stampStart=binary.indexOf('let f=Gd({requestedScope:a,resourceMetadata:_,authServerMetadata:T,clientMetadata:e.clientMetadata})',305000000);
 const stamp=slice('sdk-issuer-stamp',stampStart,binary.indexOf('if(!k){',stampStart));
 const errorStart=binary.indexOf('Rn=class extends lo{',305000000);
 const mismatch=slice('sdk-issuer-mismatch-error',errorStart+3,binary.indexOf(',rc=class',errorStart));
 const issuer=extract('callback-issuer','function jTn(','var f=',305120000);
 const manualStart=binary.indexOf('let N=(X)=>{try{let B=new URL(X)',305162178);
 const manual=slice('manual-callback-submitter',manualStart+6,binary.indexOf(';Ce=N,M.oauthCallbackSubmitters',manualStart));
 const defaultStart=binary.indexOf('vot="https://claude.ai/oauth/',260000000);
 const nativeCimdConstant=slice('native-cimd-reference-only',defaultStart,binary.indexOf(',u=void 0',defaultStart));
 const nativeCimd=JSON.parse(nativeCimdConstant.slice(4));
 const warnings=[];
 const sdkContext=vm.createContext({console:{warn:value=>warnings.push(value)},lo:Error});
 const sdk=vm.runInContext('const Rn='+mismatch+';'+oc+scope+';({filter:oc,equal:gc,stamp:async(e,p,h,a,_,T)=>{'+stamp+'return k;}})',sdkContext,{timeout:1000});
 const bound=vm.runInNewContext(binders+';({KGe,Lkr,Hkr,Oen,Hee})',{URL},{timeout:1000});
 const pure=vm.runInNewContext(ports+';({port:$e,origin:Te,cimd:kt,loopback:De})',{URL,i:{},vot:nativeCimd,act:port=>`http://localhost:${port}/callback`},{timeout:1000});
 const key='fake-server|fake-config-hash',now=1000000;
 let store={},failWrite=false;
 const maps={rejectedStoredTokens:new Map,tokenlessRejections:new Map,savedAccessTokens:new Map};
 const cacheContext=vm.createContext({Date:{now:()=>now},yt:()=>maps});
 const cacheHelpers=vm.runInContext(recent+cache+';({LTn,Xdr,Jdr})',cacheContext,{timeout:1000});
 const credentialStore={readAsync:async()=>clone(store),mutate:async operation=>{
  const next=operation(clone(store));if(!failWrite)store=clone(next);return{success:!failWrite};
 }};
 const methods=vm.runInNewContext(ports+';({'+[resolve,patch,saveClient,saveTokens,invalidate,saveDiscovery,served,clientMetadataUrl].join(',')+'})',{
  URL,Date:{now:()=>now},zn:()=>credentialStore,Ea:()=>key,le:()=>{},Ao:String,d:String,
  qg:()=>({record:()=>{}}),L9t:()=>undefined,VSi:class extends Error{},cdr:()=>{},i:{},vot:nativeCimd,
  act:port=>`http://localhost:${port}/callback`,...bound,...cacheHelpers,
 },{timeout:1000});
 const provider=(entry,config={},fields={})=>{
  store={mcpOAuth:{...(entry===undefined?{}:{[key]:clone(entry)})},mcpOAuthClientConfig:{[key]:{clientSecret:'fake-config-secret'}}};
  failWrite=false;maps.rejectedStoredTokens.clear();maps.tokenlessRejections.clear();maps.savedAccessTokens.clear();
  return Object.assign(Object.create(methods),{serverName:'fake-server',serverConfig:{type:'http',url:'https://mcp.example/mcp',...config},redirectUri:'http://localhost:31337/callback',handleRedirection:true,_flowDiscoveryStateAt:0,_servedOverrideDocuments:new WeakMap,_presented:{record:()=>{}},readCredentialStore:async()=>clone(store),logTokenPersistFailed:()=>{},...fields});
 };
 const snapshot=p=>({store:clone(store),fields:clone(Object.fromEntries(['_lastServedClientId','_lastServedAccessToken','_lastServedRefreshToken','_codeVerifier','_pendingStepUpScope','_flowDiscoveryState','_flowDiscoveryStateAt','_flowDiscoveryStateFromConfiguredUrl','registrationOfferedByDiscovery','_discoveredAuthorizationServer'].map(name=>[name,p[name]]))),savedAccessTokens:[...maps.savedAccessTokens]});
 const base={serverName:'fake-server',serverUrl:'https://mcp.example/mcp',clientId:'fake-saved-client',clientSecret:'fake-saved-secret',issuer:'https://as.example',redirectUri:'http://localhost:41111/callback',accessToken:'fake-access',refreshToken:'fake-refresh',expiresAt:2000000,scope:'old-scope',stepUpScope:'elevated',discoveryState:{authorizationServerUrl:'https://as.example',oauthMetadataFound:true,authServerMetadataUrl:null,authorizationServerMetadata:{issuer:'https://as.example',extra:'metadata'},resourceMetadata:{resource:'https://mcp.example/mcp'},tokenlessRejectedAt:900000}};
 const resolveCases=[];
 for(const [name,entry,config,fields] of [
  ['saved-client',base,{},{}],['configured-fallback',undefined,{oauth:{clientId:'configured'}},{}],['unconfigured-empty',undefined,{},{}],
  ['stored-no-secret-configured',{...base,clientSecret:undefined}, {oauth:{clientId:base.clientId}},{}],
  ['changed-redirect-unconfigured',base,{}, {redirectUri:'http://127.0.0.1:31337/callback'}],
  ['changed-redirect-configured',base,{oauth:{clientId:'configured'}},{redirectUri:'http://127.0.0.1:31337/callback'}],
  ['changed-redirect-same-configured',base,{oauth:{clientId:base.clientId}},{redirectUri:'http://127.0.0.1:31337/callback'}],
  ['legacy-no-redirect',{...base,redirectUri:undefined},{},{}],
  ['cimd-new-loopback-path',{...base,clientId:nativeCimd},{},{redirectUri:'http://localhost:31337/other'}],
  ['cimd-repair-to-configured',{...base,clientId:nativeCimd},{oauth:{clientId:'configured'}},{redirectUri:'https://app.example/callback'}],
  ['defer-registration',undefined,{},{handleRedirection:false,registrationOfferedByDiscovery:true}],
  ['noninteractive-keeps-registration',base,{},{handleRedirection:false,redirectUri:'https://app.example/callback'}],
  ['lone-client-secret',{...base,clientId:'fake\ud800client',clientSecret:'fake\udfffsecret'},{},{}],
 ]) {
  const p=provider(entry,config,fields);let result,error;
  try{result=await p.resolveClientInformation()}catch(cause){error=true}
  resolveCases.push({name,...(entry!==undefined?{entry}:{}),config,fields,...(result!==undefined?{result}:{}),...(error?{error:true}:{}),after:snapshot(p)});
 }
 const sdkIssuerCases=[];
 for(const current of ['https://as.example','https://as.example/','https://AS.example','https://as.example//','https://as.example/\ud800'])for(const stored of [undefined,'https://as.example','https://as.example/','https://other.example','https://as.example/\ufffd'])for(const canSave of [false,true]) {
  warnings.length=0;const input={client_id:'fake-client',...(stored!==undefined?{issuer:stored}:{})},writes=[];
  const p={clientMetadata:{},clientInformation:()=>input,...(canSave?{saveClientInformation:async(value,context)=>writes.push({value,context})}:{})};
  let value,error;try{value=await sdk.stamp(p,current,{issuer:current})}catch(cause){error={message:cause.message,recorded:cause.recordedIssuer,current:cause.currentIssuer}}
  sdkIssuerCases.push({current,input,canSave,equal:stored===undefined?null:sdk.equal(stored,current),...(value!==undefined?{result:value}:{}),...(error?{error}:{}),writes,warnings:[...warnings]});
 }
 const saveClientCases=[];
 for(const [name,entry,input,config,fields] of [
  ['new-client',undefined,{client_id:'new',client_secret:'fake-new-secret',issuer:'https://new.example'},{},{}],
  ['keep-previous-issuer',base,{client_id:'new'},{},{}],
  ['configured-secret-withheld',{...base,clientSecret:undefined},{client_id:base.clientId,client_secret:'fake-config-secret',issuer:'https://as.example'},{oauth:{clientId:base.clientId}},{}],
  ['noninteractive-keeps-redirect',base,{client_id:base.clientId,issuer:'https://as.example'},{},{handleRedirection:false,redirectUri:'https://other.example/callback'}],
  ['lone-source',base,{client_id:'new\ud800',client_secret:'fake\udfff',issuer:'https://as.example/\ud800'},{},{}],
 ]) { const p=provider(entry,config,fields);await p.saveClientInformation(input);saveClientCases.push({name,...(entry!==undefined?{entry}:{}),input,config,fields,after:snapshot(p)}); }
 const invalidationCases=[];
 for(const scope of ['verifier','client','tokens','all','discovery'])for(const mode of ['owned','foreign-token','foreign-client','rotated-refresh','url-only-discovery','empty-entry']) {
  const entry=mode==='empty-entry'?{serverName:'fake-server',serverUrl:'https://mcp.example/mcp',accessToken:'',extra:'keep'}:mode==='url-only-discovery'?{...base,discoveryState:{authorizationServerUrl:'https://as.example',oauthMetadataFound:true,authServerMetadataUrl:null}}:base;
  const fields={_codeVerifier:'fake-verifier',_lastServedClientId:mode==='foreign-client'?'old-client':base.clientId,_lastServedAccessToken:mode==='foreign-token'?'old-access':base.accessToken,_lastServedRefreshToken:mode==='rotated-refresh'?'old-refresh':base.refreshToken};
  const p=provider(entry,{},fields);await p.invalidateCredentials(scope);invalidationCases.push({scope,mode,entry,fields,after:snapshot(p)});
 }
 const previousStates=[undefined,{authorizationServerUrl:''},{authorizationServerUrl:'https://old.example',oauthMetadataFound:false},{authorizationServerUrl:'https://old.example',oauthMetadataFound:true},{authorizationServerUrl:'https://old.example',oauthMetadataFound:true,authServerMetadataUrl:null},{authorizationServerUrl:'https://old.example',oauthMetadataFound:true,authServerMetadataUrl:'https://old.example/metadata',xaaPinned:true}];
 const bindingCases=[];
 for(const previous of previousStates)for(const discovered of [{authorizationServerUrl:'https://new.example',oauthMetadataFound:true},{authorizationServerUrl:'https://new.example',oauthMetadataFound:false},{authorizationServerUrl:'not URL',oauthMetadataFound:true}])for(const servedMetadataUrl of [undefined,'https://new.example/metadata'])for(const interactiveSignIn of [false,true]) {
  const input={...(previous!==undefined?{previous}:{}),discovered,...(servedMetadataUrl!==undefined?{servedMetadataUrl}:{}),interactiveSignIn};
  bindingCases.push({...input,discovery:bound.Lkr(input),tokenSave:bound.Hkr({...input,signIn:discovered}),xaa:bound.Oen({previous,issuedBy:'https://new.example',interactiveSignIn}),...(previous?{stripped:bound.KGe(previous)}:{})});
 }
 const saveDiscoveryCases=[];
 for(const kind of ['metadata-registration','metadata-cimd','metadata-no-registration','url-only','xaa-skip'])for(const previous of previousStates.slice(2)) {
  const entry={...base,discoveryState:previous};
  const config=kind==='xaa-skip'?{oauth:{xaa:true}}:{oauth:{authServerMetadataUrl:'https://new.example/metadata'}};
  const input={authorizationServerUrl:'https://new.example',resourceMetadataUrl:'https://mcp.example/meta',...(kind==='url-only'?{}:{authorizationServerMetadata:{issuer:'https://new.example',...(kind==='metadata-registration'?{registration_endpoint:'https://new.example/register'}:{}),...(kind==='metadata-cimd'?{client_id_metadata_document_supported:true}:{})}})};
  const p=provider(entry,config,{_servedOverrideAuthorizationServerUrl:'https://new.example'});await p.saveDiscoveryState(input);saveDiscoveryCases.push({kind,entry,config,input,after:snapshot(p)});
 }
 const saveTokenCases=[];
 for(const interactive of [false,true])for(const input of [{access_token:'fake-access'},{access_token:'new-access',refresh_token:'new-refresh',expires_in:0,scope:'new',issuer:'https://new.example'},{access_token:'new-access',expires_in:1.25},{access_token:'new-access',expires_in:null},{access_token:'new\ud800access',refresh_token:'new\udfffrefresh',expires_in:-1.25}]) {
  const fields={handleRedirection:interactive,_pendingStepUpScope:'elevated',_flowDiscoveryState:{authorizationServerUrl:'https://new.example'},_flowDiscoveryStateAt:100,_lastServedAccessToken:'fake-access',_lastServedRefreshToken:'fake-refresh',_discoveredAuthorizationServer:{authorizationServerUrl:'https://new.example',oauthMetadataFound:true,servedMetadataUrl:'https://new.example/metadata'}};
  const p=provider(base,{},fields);await p.saveTokens(input);saveTokenCases.push({interactive,input,entry:base,fields,after:snapshot(p)});
 }
 const manualCases=[];
 for(const input of ['https://localhost/callback?code=fake&state=expected-state','https://other.example/path?code=fake&state=expected-state','not a URL','raw-code','https://localhost/callback?code=&state=expected-state','https://localhost/callback?code=fake&state=wrong','https://localhost/callback?error=denied&state=wrong','https://localhost/callback?error=denied&error_description=fake+refusal&state=expected-state','https://localhost/callback?code=first&code=second&state=expected-state&state=wrong&iss=first&iss=second','https://localhost/callback?error=denied&state=expected-state','https://localhost/callback?code=fake&state=expected-state&iss=%ED%A0%80']) {
  const observations=[];const submit=vm.runInNewContext(issuer+';'+manual,{URL,Q:'expected-state',e:'fake-server',le:()=>{},V:()=>observations.push({kind:'cleanup'}),Y:error=>observations.push({kind:'error',message:error.message}),Oe:(code,iss)=>observations.push({kind:'code',code,...(iss!==undefined?{iss}:{})})},{timeout:1000});
  manualCases.push({input,accepted:submit(input),observations});
 }
 const callbackPortCases=[];
 for(const redirectUri of ['http://localhost:31337/callback','http://127.0.0.1:31337/callback','http://LOCALHOST:31337/callback','http://localhost:031337/callback','http://localhost:80/callback','http://localhost:31337/callback/','http://localhost:31337/callback?x=1','http://localhost:31337/callback#x','https://localhost:31337/callback','http://[::1]:31337/callback','not URL'])for(const clientId of ['fake-client','']) {
  const entry={clientId,redirectUri};const result=pure.port(entry);callbackPortCases.push({entry,...(result!==undefined?{port:result}:{})});
 }
 const fixture={version:'2.1.322',native_sha256:SHA,ranges,scope:'Unchanged provider methods, SDK issuer filtering/stamping, discovery binders, manual callback submitter and saved-port helper run with fake store/time/maps. Store adapter snapshots JSON to model persistence; key adapter fixes one server account. Logging/policy telemetry and SDK Error base branding are adapters, so their side effects/branding are not claimed. Native Anthropic CIMD URL appears only as reference input, never product configuration.',resolveCases,sdkIssuerCases,saveClientCases,invalidationCases,bindingCases,saveDiscoveryCases,saveTokenCases,manualCases,callbackPortCases};
 if(artifactPath) {
  mkdirSync(artifactPath,{recursive:true});
  for(const [name,source]of Object.entries(sources))writeFileSync(path.join(artifactPath,name+'.js'),source);
  writeFileSync(path.join(artifactPath,'ranges.json'),JSON.stringify(ranges,null,2)+'\n');
 }
 return fixture;
}

if(process.argv[1]&&fileURLToPath(import.meta.url)===path.resolve(process.argv[1])) {
 const artifactIndex=process.argv.indexOf('--artifacts');const artifacts=artifactIndex<0?undefined:process.argv[artifactIndex+1];
 const fixture=await generateOAuthStateFixture(undefined,artifacts),rendered=JSON.stringify(fixture,null,2)+'\n';
 const target=new URL('../../crates/mcp/tests/fixtures/mcp_oauth_state_2_1_322.json',import.meta.url);
 if(process.argv.includes('--write'))writeFileSync(target,rendered);else assert.equal(hash(readFileSync(target)),hash(rendered),'Native OAuth state fixture changed');
 console.log(`MCP OAuth native state: ${Object.entries(fixture).filter(([key])=>key.endsWith('Cases')).reduce((count,[,cases])=>count+cases.length,0)} cases passed`);
}
