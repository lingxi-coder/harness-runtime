// MCP-server OAuth only. This never reads a model provider account or keychain.
import assert from 'node:assert/strict';
import {createHash, webcrypto} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import {homedir} from 'node:os';
import path from 'node:path';
import {parse as parseLegacyUrl,fileURLToPath} from 'node:url';
import vm from 'node:vm';
export const BASELINE_SHA256='a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450';
export const SOURCE_RANGES=[
  {
    "name": "challenge",
    "start": 305036636,
    "end": 305037193,
    "sha256": "1f40b06573b7550b98089aa4b5c48ba83145fbddb4c27158cba32c400a838b47"
  },
  {
    "name": "prm",
    "start": 305037193,
    "end": 305037652,
    "sha256": "de7699c67844872db0f8b4fecf18e140ad8fc989a95cdeb0cf3033a86d69c1a6"
  },
  {
    "name": "discoveryUrls",
    "start": 305037652,
    "end": 305039042,
    "sha256": "8aa0d44155368b8de4a19dd12e16bd2694c0193765182ef5923c7954e0676e41"
  },
  {
    "name": "asMetadata",
    "start": 305039042,
    "end": 305039688,
    "sha256": "42141f0a1b57e460f6bb1bc32184c7fce7a458f2b25f01a2504ac7a20dbe6319"
  },
  {
    "name": "discovery",
    "start": 305039688,
    "end": 305040134,
    "sha256": "b02c670e795598a7a4fa13c78cf9d6de2e4d8bfa0b9ae4e0206ba92ef108cf53"
  },
  {
    "name": "authorize",
    "start": 305040134,
    "end": 305041115,
    "sha256": "2f1acd91d04e2947762cadcc6a8be50b1eed18ec42ff336248e3b1b30868e2f8"
  },
  {
    "name": "tokenGrant",
    "start": 305041115,
    "end": 305041858,
    "sha256": "348e79291fc060185ace56913dedbf8f11a78e8d4687965ea9e99f54ff84e5e7"
  },
  {
    "name": "authMethods",
    "start": 305029809,
    "end": 305030936,
    "sha256": "e998b6a434472751fb0cf9e3ea2b5bc294f4956c88a807d8202e31b866d0bc7c"
  },
  {
    "name": "endpointPolicy",
    "start": 305030936,
    "end": 305031127,
    "sha256": "292efa5ca0a1c6646705eb56b8249842a9211e5f1e0cb62b51fd2ae90e6c85ef"
  },
  {
    "name": "clientMetadata",
    "start": 305031127,
    "end": 305031513,
    "sha256": "85aa3d00c45f81f8bfebb9bf6f44b11f2dd2bb95c68cc85d5c76f6ea7219b81a"
  },
  {
    "name": "scope",
    "start": 305032155,
    "end": 305032467,
    "sha256": "80a72cb83a2975599bb9ab1518f6fe3b310478af7575bd364f535c43002a16fa"
  },
  {
    "name": "issuerCheck",
    "start": 305028597,
    "end": 305028886,
    "sha256": "88d8daaaa15539234b1050f9bc5e5c85f5ad37e40a6d8a44323f7c884489a251"
  },
  {
    "name": "exchangeRefresh",
    "start": 305041858,
    "end": 305042523,
    "sha256": "3c92646d1794241a2a8378c75ac69d5d99eefbcf33c06a9e9583f5dbdf412897"
  },
  {
    "name": "registration",
    "start": 305043226,
    "end": 305043744,
    "sha256": "c843eeee44c9079dcf62ee4bcab00f549cf3f3c14e1afd2a589d1fe8799d0a80"
  },
  {
    "name": "revocation",
    "start": 305178419,
    "end": 305179487,
    "sha256": "9af1034d70740824a83aec1a93e658c4559c7427c41ccf9db720182998d9a8ad"
  },
  {
    "name": "callback",
    "start": 305191717,
    "end": 305192835,
    "sha256": "7b801264cade3b4ae87234cad36776d7b07fd629a8a81c8a8bb2ebc60d17813d"
  },
  {
    "name": "state",
    "start": 305199497,
    "end": 305199635,
    "sha256": "cd2f49665b8ed80279a56cd2c82542b767e538be68b9768a18aa9b409eeed65d"
  },
  {
    "name": "pkce",
    "start": 304757046,
    "end": 304757986,
    "sha256": "317590c366bb60159b042a1b6343a831d671f7dc3fcdcc2e827d20ce496ac79f"
  },
  {
    "name": "providerClientMetadata",
    "start": 305197439,
    "end": 305197786,
    "sha256": "2f6a2902c755cbfa56bf55d83d38cfb37abe2914e8b5f4cfcbcc5267cdffa5e3"
  },
  {
    "name": "providerScope",
    "start": 305226504,
    "end": 305226999,
    "sha256": "8a18cb5c052bf879405044b0dfe1ec7875262212c15d79aaf35e1b1d90bb4d7f"
  },
  {
    "name": "providerRedirect",
    "start": 305209629,
    "end": 305210393,
    "sha256": "cc3affd2074c6c2d69476fe10653b56a3f0ad80ec5e01ae07858f6af22548024"
  },
{
  "name": "callbackIssuer",
  "start": 305124928,
  "end": 305124992,
  "sha256": "96b68f1b39b5a5fbbf991f909371950d5220f3844d9a781328a1166b201e92c0"
}
];
const digest=value=>createHash('sha256').update(value).digest('hex');
export function loadNative(binaryPath=path.join(homedir(),'.local/share/claude/versions/2.1.322')) {
 const binary=readFileSync(binaryPath);assert.equal(digest(binary),BASELINE_SHA256);
 const sources=Object.fromEntries(SOURCE_RANGES.map(range=>{const bytes=binary.subarray(range.start,range.end);assert.equal(digest(bytes),range.sha256);return[range.name,bytes.toString('utf8')];}));
 const context=vm.createContext({URL,URLSearchParams,Headers,Response,Request,Buffer,TextEncoder,btoa,atob,
   fetch,At:'2025-11-25',tc:false,wn:'code',$n:'S256',t:()=>{},le:()=>{},qg:()=>({record:()=>{}}),
   XTn:async()=>({code_verifier:'fixed-verifier-43',code_challenge:'fixed-challenge'}),
   b9t:{parse:value=>value},Xr:{parse:value=>value},dPt:{parse:value=>value},S9t:{parse:value=>value},Zr:{parse:value=>value},
   sc:async response=>Error('OAuth '+(response.status??'')+' '+(await response.text())),
   cPt:class extends Error{constructor(phase,expected,received){super(`issuer mismatch (${phase}): ${expected} != ${received}`)}},
   Sc:class extends Error{constructor(url){super('insecure token endpoint: '+url)}},
   vZr:class extends Error{constructor(value){super('registration failed '+value.status);Object.assign(this,value)}},
 });
 const names=['challenge','prm','discoveryUrls','asMetadata','discovery','authorize','tokenGrant','authMethods','endpointPolicy','clientMetadata','scope','issuerCheck','exchangeRefresh','registration'];
 const native=vm.runInContext(names.map(name=>sources[name]).join('\n')+';({challenge:o8e,prm:CZr,asUrls:Qd,prmUrl:Yd,metadata:v9t,discovery:TZr,authorize:Ces,grant:kn,exchange:YSi,refresh:Tes,authMethod:Ud,authApply:Dd,endpoint:Hd,clientMetadata:Jd,scope:Gd,issuer:Pa,register:th})',context,{timeout:1000});
 const providerScope=vm.runInContext(sources.providerScope+';({offline:Lga,isMicrosoft:Hga})',vm.createContext({URL}),{timeout:1000});
 const redirect=vm.runInContext('({'+sources.providerRedirect+'return e.toString();}}).redirectToAuthorization',vm.createContext({URL,Lga:providerScope.offline,Hga:providerScope.isMicrosoft,le:()=>{},Wx:(_kind,value)=>value}),{timeout:1000});
 return {native,sources,context,redirect,providerScope};
}
export function nativeCallbackHandler(sources,options={}) {
 const observations=[];
 const context=vm.createContext({ct:parseLegacyUrl,Q:options.state??'expected-state',K:options.redirectUri??'http://localhost:31337/callback',
  CB:value=>JSON.stringify(value),Ao:value=>value,
  V:()=>observations.push({kind:'cleanup'}),
  Y:error=>observations.push({kind:'error',message:error.message}),
  Oe:(code,iss)=>observations.push({kind:'code',code:String(code),...(iss!==undefined?{iss}:{})}),
 });
 return {handler:vm.runInContext(sources.callbackIssuer+';'+sources.callback,context,{timeout:1000}),observations};
}
export function nativeRevocation(sources, post) {
 const axios={post,isAxiosError:error=>error?.isAxiosError===true};
 return vm.runInContext(sources.revocation+';Ne',vm.createContext({URL,URLSearchParams,Buffer,encodeURIComponent,
  Mt:axios,BTn:()=>false,ocr:()=>({}),le:()=>{},qg:()=>({record:()=>{}})}),{timeout:1000});
}
export async function generateFixture(binaryPath) {
 const {native,sources,redirect}=loadNative(binaryPath);
 const pkceCases=[];
 for(const seed of [0,1,17,65,127,255]) {
  const bytes=Array.from({length:43},(_,index)=>(seed+index)%256);
  const context=vm.createContext({TextEncoder,Uint8Array,btoa,crypto:{getRandomValues:array=>{array.set(bytes.slice(0,array.length));return array},subtle:webcrypto.subtle}});
  const actual=await vm.runInContext(sources.pkce+';XTn()',context,{timeout:1000});
  pkceCases.push({bytes,...actual});
 }
 const stateBytes=Array.from({length:32},(_,index)=>index);
 const state=await vm.runInContext('({'+sources.state+'}).state()',vm.createContext({nt:size=>{assert.equal(size,32);return Buffer.from(stateBytes)},le:()=>{}}),{timeout:1000});
 const challengeCases=[];
 for(const header of ['Bearer resource_metadata="https://RS.EXAMPLE:443", scope="read write"','Bearer  scope="read"',
  'bearer scope=x, error=invalid_token','Bearer scope=""','Bearer resource_metadata="not a url"','Basic realm="x"',
  'Bearer resource_metadata="http://localhost:31337/meta?x=1#hash",error_description="try again"']) {
  const value=native.challenge(new Response('',{status:401,headers:{'WWW-Authenticate':header}}));
  challengeCases.push({header,expected:{...(value.resourceMetadataUrl?{resource_metadata_url:value.resourceMetadataUrl.href}:{}),
   ...(value.scope!==undefined?{scope:value.scope}:{}),...(value.error!==undefined?{error:value.error}:{}),...(value.errorDescription!==undefined?{error_description:value.errorDescription}:{})}});
 }
 const discoveryUrlCases=[];
 for(const base of ['https://as.example/','https://as.example/tenant/a','https://as.example/tenant/a/?x=1#frag','http://localhost:31337/mcp?aud=one%20two']) {
  const url=new URL(base);const prm=new URL(native.prmUrl('oauth-protected-resource',url.pathname),url);prm.search=url.search;
  discoveryUrlCases.push({base,prm:prm.href,authorization:native.asUrls(base).map(value=>value.url.href)});
 }
 const discoveryOutcomeCases=[];
 for(const status of [401,404,502,500,200]) {
  let error=false,metadata=false;
  try{const result=await native.discovery('http://localhost:31337/mcp',{fetchFn:async url=>new Response(new URL(url).pathname.includes('oauth-protected-resource')?'':status===200?'{':'',{status:new URL(url).pathname.includes('oauth-protected-resource')?404:status}),skipIssuerMetadataValidation:true});metadata=result.authorizationServerMetadata!==undefined}catch{error=true}
  discoveryOutcomeCases.push({status,error,metadata});
 }
 const fallbackEndpointCases=[];
 for(const base of ['http://localhost:31337/mcp','https://as.example/tenant/path?old=1#frag','https://AS.EXAMPLE:443/']) {
  const requests=[];const fetchFn=async(url,init)=>{requests.push({url:String(url),body:String(init.body)});return new Response(init.body instanceof URLSearchParams?'{"access_token":"fake-access","token_type":"Bearer"}':'{"client_id":"fake-dcr-client"}',{status:200})};
  const {authorizationUrl}=await native.authorize(base,{clientInformation:{client_id:'fake-client'},redirectUrl:'http://localhost:31337/callback',state:'expected-state'});
  await native.register(base,{clientMetadata:{client_name:'LingXi (fake-server)',redirect_uris:['http://localhost:31337/callback']},fetchFn});
  await native.exchange(base,{clientInformation:{client_id:'fake-client'},authorizationCode:'fake-code',codeVerifier:'fixed-verifier-43',redirectUri:'http://localhost:31337/callback',fetchFn});
  fallbackEndpointCases.push({base,authorize:authorizationUrl.href,registration:requests[0].url,token:requests[1].url});
 }
 const authorizeCases=[];
 for(const endpoint of ['https://as.example/authorize','https://as.example/authorize?keep=one%20two&client_id=old&client_id=duplicate#frag',
  'https://login.microsoftonline.com/tenant/authorize?prompt=select_account&prompt=consent','https://as.example/authorize?prompt=login&prompt=select_account'])
 for(const scope of ['', 'read write','read offline_access']) {
  const metadata={authorization_endpoint:endpoint,response_types_supported:['code'],code_challenge_methods_supported:['S256']};
  const {authorizationUrl}=await native.authorize('https://as.example',{metadata,clientInformation:{client_id:'fake +~!*client'},redirectUrl:'http://localhost:31337/callback',scope,state:'expected-state',resource:new URL('https://mcp.example/dir?a=1&b=2')});
  const rendered=await redirect.call({_pendingStepUpScope:false,serverConfig:{},_metadata:{}},authorizationUrl);
  authorizeCases.push({endpoint,client_id:'fake +~!*client',redirect_uri:'http://localhost:31337/callback',scope,state:'expected-state',challenge:'fixed-challenge',resource:'https://mcp.example/dir?a=1&b=2',expected:rendered});
 }
 const scopeCases=[];
 for(const inputs of [
  {}, {client_scope:''}, {client_scope:'curated'}, {resource_scopes:['read','write'],client_scope:'curated'},
  {requested:'step up',resource_scopes:['read'],client_scope:'curated'},
  {requested:'',resource_scopes:[],client_scope:'curated'},
  {resource_scopes:['read'],configured_scope:'configured'},
  {resource_scopes:['read'],configured_scope:''},
  {resource_scopes:['read'],configured_scope:'configured',pending:true},
  {client_scope:'read offline_access'},
 ])for(const offline of [false,true]) {
  const metadata={authorization_endpoint:'https://as.example/authorize?prompt=login&prompt=select_account',response_types_supported:['code'],code_challenge_methods_supported:['S256'],scopes_supported:offline?['read','offline_access']:['read']};
  const scope=native.scope({requestedScope:inputs.requested,resourceMetadata:inputs.resource_scopes?{scopes_supported:inputs.resource_scopes}:undefined,authServerMetadata:metadata,clientMetadata:{scope:inputs.client_scope,grant_types:['authorization_code','refresh_token']}});
  const {authorizationUrl}=await native.authorize('https://as.example',{metadata,clientInformation:{client_id:'fake-client'},redirectUrl:'http://localhost:31337/callback',scope,state:'expected-state'});
  const finalUrl=await redirect.call({_pendingStepUpScope:inputs.pending??false,serverConfig:{oauth:inputs.configured_scope!==undefined?{authServerMetadataUrl:'https://as.example/metadata'}:{}},_metadata:metadata,getCuratedMetadataScope:()=>inputs.configured_scope},authorizationUrl);
  scopeCases.push({...inputs,as_scopes:metadata.scopes_supported,...(scope!==undefined?{sdk_scope:scope}:{}),expected:finalUrl});
 }
 const authCases=[];
 for(const secret of [undefined,'','fake +/~é','fake𐐀'])for(const supported of [[],['none'],['client_secret_basic'],['client_secret_post'],['client_secret_post','client_secret_basic']]) {
  const client={client_id:'fake +~!*client',...(secret!==undefined?{client_secret:secret}:{})};
  const method=native.authMethod(client,supported),headers=new Headers,body=new URLSearchParams({grant_type:'refresh_token',refresh_token:'fake +~!*refresh'});
  let error=false;try{native.authApply(method,client,headers,body)}catch{error=true}
  authCases.push({client,supported,method,headers:Object.fromEntries(headers),body:body.toString(),...(error?{error}:{})});
 }
 const grantCases=[];
 for(const kind of ['code','refresh'])for(const resource of [undefined,'https://mcp.example/mcp?space=one two']) {
  const requests=[];const fetchFn=async(url,init)=>{requests.push({url:String(url),method:init.method,headers:Object.fromEntries(new Headers(init.headers)),body:String(init.body)});return new Response(JSON.stringify({access_token:'fake-access',token_type:'Bearer',expires_in:60}),{status:200})};
  const common={metadata:{token_endpoint:'https://as.example/token',token_endpoint_auth_methods_supported:['none']},clientInformation:{client_id:'fake +~!*client'},...(resource?{resource:new URL(resource)}:{}),fetchFn};
  if(kind==='code')await native.exchange('https://as.example',{...common,authorizationCode:'fake +~!*code',codeVerifier:'fixed-verifier-43',redirectUri:'http://localhost:31337/callback'});
  else await native.refresh('https://as.example',{...common,refreshToken:'fake +~!*refresh'});
  grantCases.push({kind,resource:resource?new URL(resource).href:null,...requests[0]});
 }
 const registrationCases=[];
 for(const redirect_uri of ['http://localhost:31337/callback','http://127.0.0.1:31337/callback','https://app.example/callback','lingxi://callback'])for(const scope of [undefined,'read write'])for(const initialScope of [undefined,'curated initial']) {
  const base={client_name:'LingXi (fake-server)',redirect_uris:[redirect_uri],grant_types:['authorization_code','refresh_token'],response_types:['code'],token_endpoint_auth_method:'none',...(initialScope!==undefined?{scope:initialScope}:{})};
  const metadata=native.clientMetadata({clientMetadata:base,redirectUrl:redirect_uri});const requests=[];
  await native.register('https://as.example',{metadata:{registration_endpoint:'https://as.example/register'},clientMetadata:metadata,scope,fetchFn:async(url,init)=>{requests.push({url:String(url),headers:Object.fromEntries(new Headers(init.headers)),body:String(init.body)});return new Response('{"client_id":"fake-dcr-client"}',{status:201})}});
  registrationCases.push({redirect_uri,...(scope!==undefined?{scope}:{}),...(initialScope!==undefined?{initial_scope:initialScope}:{}),...requests[0]});
 }
 const revocationCases=[];
 for(const auth_method of ['client_secret_basic','client_secret_post'])for(const client_secret of [undefined,'','fake +~!*é'])for(const retry of [false,true]) {
  const requests=[];const revoke=nativeRevocation(sources,async(url,body,options)=>{
   requests.push({url,headers:Object.fromEntries(new Headers(options.headers)),body:body.toString()});
   if(retry&&requests.length===1)throw Object.assign(Error('fake unauthorized'),{isAxiosError:true,response:{status:401}});
  });
  await revoke({serverName:'fake-server',endpoint:'https://as.example/revoke',token:'fake +~!*token',tokenTypeHint:'refresh_token',clientId:'fake +~!*client',clientSecret:client_secret,accessToken:'fake-access',authMethod:auth_method,configuredHeaders:{}});
  revocationCases.push({auth_method,...(client_secret!==undefined?{client_secret}:{}),retry,requests});
 }
 const callbackCases=[];
 for(const requestUrl of ['/callback?code=fake-code&state=expected-state','/callback?code=fake-code&state=wrong',
  '/callback?code=fake-code&state=expected-state&state=expected-state','/callback?code=first&code=second&state=expected-state',
  '/callback?code=&state=expected-state','/callback?code=fake-code&error=&state=expected-state',
  '/callback?error=access_denied&error_description=fake%20refusal&error_uri=https%3A%2F%2Fas.example%2Ferror&state=expected-state',
  '/callback?code=fake-code&state=expected-state&iss=https%3A%2F%2Fas.example','/callback?code=fake-code&state=expected-state&iss=one&iss=two','/favicon.ico']) {
  const {handler,observations}=nativeCallbackHandler(sources);let status,body;
  handler({url:requestUrl},{writeHead:value=>{status=value},end:value=>{body=JSON.parse(value)}});
  callbackCases.push({request_url:requestUrl,...(status!==undefined?{status}:{}),...(body!==undefined?{page:body}:{}),observations});
 }
 return {version:'2.1.322',native_sha256:BASELINE_SHA256,sourceByteRanges:SOURCE_RANGES,
  scope:'MCP OAuth only. Unchanged native SDK/application helpers execute in isolated JS contexts with fake data and caller-supplied fetch. Schema parsers and error constructors are adapters; auth-failure presence is compared without host-specific btoa error prose. Schema validation and whole-CLI account login are not claimed. Branded client_name stays LingXi and provider-neutral DCR remains default; Anthropic-hosted CIMD is not adopted.',
  pkceCases,stateCase:{bytes:stateBytes,expected:state},challengeCases,discoveryUrlCases,discoveryOutcomeCases,fallbackEndpointCases,authorizeCases,scopeCases,authCases,grantCases,registrationCases,revocationCases,callbackCases};
}
if(process.argv[1]&&fileURLToPath(import.meta.url)===path.resolve(process.argv[1])) {
 const args=process.argv.slice(2);const binaryPath=args.find(arg=>!arg.startsWith('--'));
 const fixture=await generateFixture(binaryPath);const rendered=JSON.stringify(fixture,null,2)+'\n';
 const fixturePath=new URL('../../crates/mcp/tests/fixtures/mcp_oauth_2_1_322.json',import.meta.url);
 if(args.includes('--write'))writeFileSync(fixturePath,rendered);else assert.equal(digest(readFileSync(fixturePath)),digest(rendered),'MCP OAuth native fixture changed');
 console.log(`Claude Code2.1.322 MCP OAuth: ${fixture.pkceCases.length} PKCE, ${fixture.challengeCases.length} challenge, ${fixture.discoveryUrlCases.length} discovery URL, ${fixture.discoveryOutcomeCases.length} discovery outcome, ${fixture.fallbackEndpointCases.length} fallback, ${fixture.authorizeCases.length} authorize, ${fixture.scopeCases.length} scope, ${fixture.authCases.length} auth, ${fixture.grantCases.length} grant, ${fixture.registrationCases.length} DCR, ${fixture.revocationCases.length} revoke, ${fixture.callbackCases.length} callback cases verified`);
}
