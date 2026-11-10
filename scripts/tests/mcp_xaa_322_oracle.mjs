// Hash-pinned MCP XAA helpers with their actual bundled Zod constructors.
// Fake MCP/IdP data only; no account credential or native credential store.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import {homedir} from 'node:os';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import vm from 'node:vm';
import {loadNativeOAuthSchemas,BASELINE_SHA256} from './mcp_oauth_322_schema_oracle.mjs';
import {loadNative} from './mcp_oauth_322_oracle.mjs';

export async function generateXaaFixture(binaryPath=path.join(homedir(),'.local/share/claude/versions/2.1.322')) {
 const binary=readFileSync(binaryPath),hash=value=>createHash('sha256').update(value).digest('hex');
 assert.equal(hash(binary),BASELINE_SHA256);
 const ranges=[];
 const slice=(name,start,end)=>{const bytes=binary.subarray(start,end);ranges.push({name,start,end,sha256:hash(bytes)});return bytes.toString('utf8')};
 const xaaSource=slice('xaa-native-helpers',305164488,305171120);
 const issuerSource=slice('idp-issuer-key',305126856,305127017);
 const jwtSource=slice('idp-jwt-exp',305129110,305129286);
 const oidcSource=slice('idp-oidc-discovery',305128236,305129110);
 const saveStart=binary.indexOf('async function sRa(',305126856),saveEnd=binary.indexOf('async function SZr(',saveStart);
 const saveSource=slice('idp-save-jwt-expiry',saveStart,saveEnd);
 const cacheReadStart=binary.indexOf('async function fdr(',305126856),cacheReadEnd=binary.indexOf('async function k(',cacheReadStart);
 const cacheReadSource=slice('idp-cache-read',cacheReadStart,cacheReadEnd);
 const {schemas,dependencies}=loadNativeOAuthSchemas(binaryPath);
 const {native:sdk}=loadNative(binaryPath);
 const requests=[];
 let fetchFn=async()=>{throw Error('no fake response installed')};
 const context=vm.createContext({URL,URLSearchParams,Buffer,AbortSignal,fetch:(...args)=>fetchFn(...args),...dependencies,
  p:factory=>factory,CZ:factory=>factory,_:JSON.stringify,_a:()=>({}),AB:error=>{throw error},
  je:()=>false,Ao:value=>String(value),T:class extends Error{},qg:()=>({record:()=>{}}),le:()=>{},
  CZr:sdk.prm,v9t:sdk.metadata,
 });
 const native=vm.runInContext(xaaSource+';({normalize:re,redact:ne,exchange:rt,bearer:it,flow:Ae,prm:et,authorization:tt,exchangeSchema:Ze,bearerSchema:Qe})',context,{timeout:3000});
 const issuer=vm.runInNewContext(issuerSource+';yTn',{URL},{timeout:1000});
 const jwt=vm.runInNewContext(jwtSource+';A',{Buffer,ee:JSON.parse},{timeout:1000});
 const oidc=vm.runInNewContext(oidcSource+';pdr',{URL,S9t:schemas.oidc,Ao:value=>String(value),T:class extends Error{},P:(...args)=>fetchFn(...args)},{timeout:1000});
 const normalizeCases=[];
 for(const input of ['https://AS.Example:443/','https://AS.Example:443/tenant///','https://AS.Example:443/tenant///?x=1#frag',
  'https://USER:Pass@AS.Example:443/tenant///','https://AS.Example/one/../two/','https://as.example/🐈/',
  'https://as.example/?x=1','not a URL///','lingxi://Callback/root///'])normalizeCases.push({input,xaa:native.normalize(input),issuer:issuer(input)});
 const jwtCases=[];
 for(const claims of [{exp:2000000000},{exp:0},{exp:-1.25},{exp:2000000000.125},{exp:'2000000000'},{},null]) {
  const token='header.'+Buffer.from(JSON.stringify(claims)).toString('base64url')+'.signature';let value;
  try{value=jwt(token)}catch{}
  jwtCases.push({token,...(value!==undefined?{expected:value}:{})});
 }
 for(const token of ['not-jwt','a.b','a.b.c.d','header.eyJleHAiOjIwMDAwMDAwMDB9==.sig','header.eyJleHAiOjIwMDAwMDAwMDB9!!?.sig']) {
  const value=jwt(token);jwtCases.push({token,...(value!==undefined?{expected:value}:{})});
 }
 const cacheExpiryCases=[];
 for(const claims of ['{"exp":0}','{"exp":-1.25}','{"exp":2000000000.125}','{"exp":1e400}','{"sub":"fake"}']) {
  const token='header.'+Buffer.from(claims).toString('base64url')+'.signature';
  let stored;
  const save=vm.runInNewContext(saveSource+';sRa',{A:jwt,Date:{now:()=>1000000},k:async(issuer,token,expires)=>{stored=JSON.stringify({idToken:token,expiresAt:expires})}},{timeout:1000});
  const expiry=await save('https://idp.example',token);
  cacheExpiryCases.push({token,expected_json:JSON.stringify(expiry),...(Number.isFinite(expiry)?{expected:expiry}:{nonfinite:String(expiry)}),stored_json:stored});
 }
 const cacheMillisecondsCases=[];
 for(const raw of [undefined,'null','0','1060000','1060000.0000000002','545531393461.19696']) {
  const entry={idToken:'fake-token',...(raw!==undefined?{expiresAt:JSON.parse(raw)}:{})};
  const read=vm.runInNewContext(cacheReadSource+';fdr',{zn:()=>({readAsync:async()=>({mcpXaaIdp:{[issuer('https://idp.example')]:entry}})}),yTn:issuer,Date:{now:()=>1000000},X:60},{timeout:1000});
  cacheMillisecondsCases.push({...(raw!==undefined?{raw_expiry_json:raw}:{}),cached:await read('https://idp.example')!==undefined});
 }
 const redactionCases=[];
 for(const input of ['{"subject_token":"fake","scope":"one two"}', '{"access_token"\uFEFF:\u00A0"fake"}',
  '{"access_token"\u0085:\u0085"fake"}','{"client_secret":"a\\"tail"}', 'plain 🐈 text',
  '{"refresh_token":"fake","id_token":"fake","assertion":"fake"}'])redactionCases.push({input,expected:native.redact(input)});
 const wireCases=[];
 const captureResponse=(status,body)=>async(url,init={})=>{
  requests.push({url:String(url),headers:Object.fromEntries(new Headers(init.headers)),body:String(init.body)});
  return new Response(body,{status});
 };
 const baseExchange={tokenEndpoint:'https://idp.example/token',audience:'https://as.example/issuer',resource:'https://mcp.example/mcp',idToken:'fake +~!*id-token',clientId:'fake +~!*client',scope:'ignored scope'};
 const baseBearer={tokenEndpoint:'https://as.example/token',assertion:'fake +~!*assertion',clientId:'fake +~!*client',clientSecret:'fake +~!*é',scope:'ignored scope'};
 for(const kind of ['exchange','bearer'])for(const secret of [undefined,'','fake +~!*é'])for(const method of ['client_secret_basic','client_secret_post']) {
  requests.length=0;
  const input=kind==='exchange'?{...baseExchange,...(secret!==undefined?{clientSecret:secret}:{})}:{...baseBearer,clientSecret:secret??'fake +~!*é',authMethod:method};
  const body=kind==='exchange'?'{"access_token":"fake-id-jag","issued_token_type":"urn:ietf:params:oauth:token-type:id-jag","expires_in":"1.25","extra":"strip"}':'{"access_token":"fake-access","expires_in":"1.25","extra":"strip"}';
  const result=await native[kind==='exchange'?'exchange':'bearer']({...input,fetchFn:captureResponse(200,body)});
  wireCases.push({kind,input,request:requests[0],expected:result});
 }
 const responseCases=[];
 for(const [status,body] of [[400,'{"subject_token":"fake","error":"denied"}'],[500,'🐈'.repeat(150)],[500,'A'+'🐈'.repeat(150)],
  [200,'not JSON'],[200,'{}'],[200,'{"access_token":"","issued_token_type":"urn:ietf:params:oauth:token-type:id-jag"}'],
  [200,'{"access_token":"fake","issued_token_type":"wrong"}'],
  [200,'{"access_token":"fake","issued_token_type":"urn:ietf:params:oauth:token-type:id-jag","expires_in":{}}']]) {
  for(const kind of ['exchange','bearer']) {
   requests.length=0;let error;
   try{await native[kind==='exchange'?'exchange':'bearer']({...kind==='exchange'?baseExchange:baseBearer,fetchFn:captureResponse(status,body)})}
   catch(cause){error={message_units:Array.from({length:cause.message.length},(_,index)=>cause.message.charCodeAt(index)),...(cause.shouldClearIdToken!==undefined?{should_clear:cause.shouldClearIdToken}:{})}}
   responseCases.push({kind,status,body,...(error?{error}:{})});
  }
 }
 const sourceCases=[];
 for(const kind of ['exchange','bearer'])for(const body of [
  '{"access_token":"\\ud800","issued_token_type":"urn:ietf:params:oauth:token-type:id-jag","scope":"\\udfff","refresh_token":"\\ud800","expires_in":"1.25"}',
  '{"access_token":"fake","issued_token_type":"urn:ietf:params:oauth:token-type:id-jag","token_type":"Bearer","extra":"strip"}',
 ]) {
  const schema=native[kind==='exchange'?'exchangeSchema':'bearerSchema']();
  sourceCases.push({kind,body,expected_json:JSON.stringify(schema.parse(JSON.parse(body)))});
 }
 const secretSourceCases=[];
 for(const source_json of ['"\\ud800"','"\\ufffd"','"fake\\ud800"','"é"'])for(const method of ['client_secret_basic','client_secret_post']) {
  requests.length=0;let error;
  try{await native.bearer({...baseBearer,clientSecret:JSON.parse(source_json),authMethod:method,fetchFn:captureResponse(200,'{"access_token":"fake-access"}')})}
  catch(cause){error=cause.name}
  secretSourceCases.push({source_json,method,...(requests[0]?{request:requests[0]}:{}),...(error?{error_name:error}:{})});
 }
 const oidcCases=[];
 const metadata={issuer:'https://idp.example',authorization_endpoint:'https://idp.example/authorize',token_endpoint:'https://idp.example/token',jwks_uri:'https://idp.example/keys',response_types_supported:['code'],subject_types_supported:['public'],id_token_signing_alg_values_supported:['RS256']};
 for(const input of ['https://idp.example/tenant','https://idp.example/tenant/?old=1#frag','https://IDP.EXAMPLE:443/','not URL']) {
  requests.length=0;fetchFn=async(url,init)=>{requests.push({url:String(url),headers:Object.fromEntries(new Headers(init.headers))});return new Response(JSON.stringify(metadata),{status:200})};
  let result,error;try{result=await oidc(input)}catch(cause){error=cause.message}
  oidcCases.push({input,...(requests[0]?{request:requests[0]}:{}),...(result?{expected:result}:{}),...(error?{error}:{} )});
 }
 return {version:'2.1.322',native_sha256:BASELINE_SHA256,ranges,scope:'MCP XAA only. Native Ze/Qe and S9t execute with actual bundled Zod. Only lazy factory/logging/fetch adapters are supplied; no identity schema parsers. Fake credentials; no global account or keychain access.',normalizeCases,jwtCases,cacheExpiryCases,cacheMillisecondsCases,redactionCases,wireCases,responseCases,sourceCases,secretSourceCases,oidcCases};
}

if(process.argv[1]&&fileURLToPath(import.meta.url)===path.resolve(process.argv[1])) {
 const fixture=await generateXaaFixture();const rendered=JSON.stringify(fixture,null,2)+'\n';
 const target=new URL('../../crates/mcp/tests/fixtures/mcp_xaa_2_1_322.json',import.meta.url);
 if(process.argv.includes('--write'))writeFileSync(target,rendered);else assert.equal(readFileSync(target,'utf8'),rendered);
 console.log(`MCP XAA native fixture passed: ${Object.entries(fixture).filter(([key])=>key.endsWith('Cases')).reduce((sum,[,value])=>sum+value.length,0)} cases`);
}
