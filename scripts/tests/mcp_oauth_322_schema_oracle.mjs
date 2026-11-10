// Exact bundled OAuth schemas and Zod dependencies from Claude Code 2.1.322.
// No schema adapters, account credentials, network requests or SDK processes.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import {homedir} from 'node:os';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import vm from 'node:vm';

export const BASELINE_SHA256='a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450';
const hash=bytes=>createHash('sha256').update(bytes).digest('hex');

export function loadNativeOAuthSchemas(binaryPath=path.join(homedir(),'.local/share/claude/versions/2.1.322'),binaryBytes) {
 const binary=binaryBytes??readFileSync(binaryPath);assert.equal(hash(binary),BASELINE_SHA256);
 const ranges=[];
 const slice=(name,start,end)=>{const bytes=binary.subarray(start,end);ranges.push({name,start,end,sha256:hash(bytes)});return bytes.toString('utf8')};
 function pureModule(name,start,end,imports={}) {
  const source=slice(name,start,end).replace(/import\{[^}]+\}from"[^"]+";/g,'').replace(/export\{([^}]+)\};/,'return {$1};');
  return vm.runInNewContext(`(()=>{${source}})()`,{URL,URLSearchParams,...imports},{timeout:3000});
 }
 const core=pureModule('bundled-zod-core',260913877,260925569);
 const constructors=pureModule('bundled-zod-constructors',260925570,261020487,core);
 const numbers=pureModule('bundled-zod-coercion',261243918,261244653,constructors);
 const sdkStart=304763858,sdkEnd=305102431,sdk=binary.subarray(sdkStart,sdkEnd);
 const lazyStart=sdk.indexOf('function tt(e)'),lazyEnd=sdk.indexOf('var rt=',lazyStart);
 const schemasStart=sdk.indexOf('Re=Grt()'),schemasEnd=sdk.indexOf(';var on=',schemasStart);
 assert(lazyStart>0&&lazyEnd>lazyStart&&schemasStart>0&&schemasEnd>schemasStart);
 const lazy=slice('native-lazy-schema',sdkStart+lazyStart,sdkStart+lazyEnd);
 const schemaSource=slice('native-oauth-schema-definitions',sdkStart+schemasStart,sdkStart+schemasEnd);
 const schemas=vm.runInNewContext(`${lazy};var ${schemaSource};({prm:Xr,authorization:dPt,oidc:S9t,tokens:b9t,clientMetadata:oa,clientInformation:sa,registration:Zr,error:w9t,registrationError:Xa,revocation:Za,idJag:ra, Xr,dPt,S9t,b9t,Zr,oa,sa,w9t,Xa,Za,ra})`,{URL,...core,...constructors,...numbers},{timeout:3000});
 return {schemas,ranges,dependencies:{...core,...constructors,...numbers},schemaSource,lazy};
}

export function generateSchemaFixture(binaryPath) {
 const {schemas,ranges}=loadNativeOAuthSchemas(binaryPath);const cases=[];
 const base={
  prm:{resource:'https://mcp.example/mcp'},
  authorization:{issuer:'https://as.example',authorization_endpoint:'https://as.example/authorize',token_endpoint:'https://as.example/token',response_types_supported:['code']},
  oidc:{issuer:'https://as.example',authorization_endpoint:'https://as.example/authorize',token_endpoint:'https://as.example/token',jwks_uri:'https://as.example/keys',response_types_supported:['code'],subject_types_supported:['public'],id_token_signing_alg_values_supported:['RS256']},
  tokens:{access_token:'fake-access',token_type:'Bearer'},
  clientMetadata:{redirect_uris:['http://localhost:31337/callback']},
  clientInformation:{client_id:'fake-client'},
  registration:{redirect_uris:['http://localhost:31337/callback'],client_id:'fake-client'},
  error:{error:'invalid_grant'},registrationError:{error:'invalid_client_metadata'},revocation:{token:'fake-token'},
  idJag:{issued_token_type:'urn:ietf:params:oauth:token-type:id-jag',access_token:'fake-id-jag'},
 };
 const add=(schema,name,value,raw)=>{
  const input=raw??JSON.stringify(value);let result;
  try{const parsed=schemas[schema].safeParse(JSON.parse(input));result=parsed.success?{success:true,output:JSON.stringify(parsed.data)}:{success:false,issues:JSON.parse(JSON.stringify(parsed.error.issues))}}
  catch(error){result={success:false,threw:error.name}}
  cases.push({schema,name,input,...result});
 };
 for(const [schema,value] of Object.entries(base)) {
  add(schema,'minimal',value);add(schema,'unknown-fields',{...value,extra:'kept?',nested:{a:1}});
  add(schema,'lone-surrogate-value-and-key',null,JSON.stringify(value).slice(0,-1)+',"extra":"\\ud800","\\udfff":"value"}');
  add(schema,'prototype-name-fields',null,JSON.stringify(value).slice(0,-1)+',"__proto__":{"polluted":true},"constructor":"kept?","toString":null}');
  for(const invalid of [null,[],true,1,'text'])add(schema,'wrong-root-'+JSON.stringify(invalid),invalid);
  for(const key of Object.keys(value)) {
   const missing={...value};delete missing[key];add(schema,'missing-'+key,missing);
   for(const invalid of [null,{},1,false])add(schema,'wrong-'+key+'-'+JSON.stringify(invalid),{...value,[key]:invalid});
  }
 }
 const strings={
  prm:['resource_name','resource_documentation'],
  authorization:['issuer','introspection_endpoint'],
  oidc:['issuer','service_documentation'],
  tokens:['access_token','token_type','id_token','scope','refresh_token'],
  clientMetadata:['token_endpoint_auth_method','application_type','client_name','scope','policy_uri','software_id','software_version','software_statement'],
  clientInformation:['client_id','client_secret'],registration:['client_id','client_secret'],
  error:['error','error_description','error_uri'],registrationError:['error','error_description'],revocation:['token','token_type_hint'],
 };
 for(const [schema,fields] of Object.entries(strings))for(const field of fields)for(const value of ['',null,1,'text'])add(schema,field+'-'+JSON.stringify(value),{...base[schema],[field]:value});
 for(const [schema,fields] of Object.entries(strings))for(const field of fields)add(schema,field+'-lone-surrogate',null,JSON.stringify({...base[schema],[field]:'LONE'}).replace('"LONE"','"\\ud800"'));
 const arrays={
  prm:['authorization_servers','scopes_supported','bearer_methods_supported','resource_signing_alg_values_supported','authorization_details_types_supported','dpop_signing_alg_values_supported'],
  authorization:['scopes_supported','response_types_supported','response_modes_supported','grant_types_supported','token_endpoint_auth_methods_supported','token_endpoint_auth_signing_alg_values_supported','revocation_endpoint_auth_methods_supported','revocation_endpoint_auth_signing_alg_values_supported','introspection_endpoint_auth_methods_supported','introspection_endpoint_auth_signing_alg_values_supported','code_challenge_methods_supported'],
  oidc:['scopes_supported','response_types_supported','response_modes_supported','grant_types_supported','acr_values_supported','subject_types_supported','id_token_signing_alg_values_supported','id_token_encryption_alg_values_supported','id_token_encryption_enc_values_supported','userinfo_signing_alg_values_supported','userinfo_encryption_alg_values_supported','userinfo_encryption_enc_values_supported','request_object_signing_alg_values_supported','request_object_encryption_alg_values_supported','request_object_encryption_enc_values_supported','token_endpoint_auth_methods_supported','token_endpoint_auth_signing_alg_values_supported','display_values_supported','claim_types_supported','claims_supported','claims_locales_supported','ui_locales_supported','code_challenge_methods_supported'],
  clientMetadata:['redirect_uris','grant_types','response_types','contacts'],
 };
 for(const [schema,fields] of Object.entries(arrays))for(const field of fields)for(const value of [[],['one','two'],[null],null,'one'])add(schema,field+'-'+JSON.stringify(value),{...base[schema],[field]:value});
 const urls={prm:['resource','jwks_uri','resource_policy_uri','resource_tos_uri'],authorization:['authorization_endpoint','token_endpoint','registration_endpoint','service_documentation','revocation_endpoint'],oidc:['authorization_endpoint','token_endpoint','userinfo_endpoint','jwks_uri','registration_endpoint','op_policy_uri','op_tos_uri'],clientMetadata:['client_uri','logo_uri','tos_uri','jwks_uri']};
 for(const [schema,fields] of Object.entries(urls))for(const field of fields)for(const value of ['','not url','HTTPS://EXAMPLE.COM:443/one','javascript:alert(1)','data:text/plain,x','vbscript:msgbox(1)','mailto:fake@example.com','lingxi://callback'])add(schema,field+'-'+JSON.stringify(value),{...base[schema],[field]:value});
 for(const schema of ['authorization','oidc'])for(const field of ['client_id_metadata_document_supported','authorization_response_iss_parameter_supported'])for(const value of [true,false,null,'true',1])add(schema,field+'-'+JSON.stringify(value),{...base[schema],[field]:value});
 for(const field of ['tls_client_certificate_bound_access_tokens','dpop_bound_access_tokens_required'])for(const value of [true,false,null,'true',1])add('prm',field+'-'+JSON.stringify(value),{...base.prm,[field]:value});
 for(const field of ['claims_parameter_supported','request_parameter_supported','request_uri_parameter_supported','require_request_uri_registration'])for(const value of [true,false,null,'true',1])add('oidc',field+'-'+JSON.stringify(value),{...base.oidc,[field]:value});
 for(const value of [null,false,true,0,-1,1.5,'','  ','\uFEFF2','0x10','0b11','0o17','1e2','Infinity','NaN','inf',[],[1],[[]],[null],[true],{},[1,2]])add('tokens','expires_in-'+JSON.stringify(value),{...base.tokens,expires_in:value});
 add('tokens','expires_in-overflow',null,'{"access_token":"fake","token_type":"Bearer","expires_in":1e999}');
 for(const schema of ['clientInformation','registration'])for(const field of ['client_id_issued_at','client_secret_expires_at'])for(const value of [null,'1',0,-1,1.25])add(schema,field+'-'+JSON.stringify(value),{...base[schema],[field]:value});
 for(const value of [null,{},[],{keys:[]},{keys:[null]},'text',1])add('clientMetadata','jwks-'+JSON.stringify(value),{...base.clientMetadata,jwks:value});
 return {version:'2.1.322',native_sha256:BASELINE_SHA256,ranges,scope:'Unchanged native OAuth schema definitions and complete bundled Zod dependencies; validation and transformed output are real native code. Input JSON and normalized output retain lone UTF16 units. No schema stubs.',cases};
}

if(process.argv[1]&&fileURLToPath(import.meta.url)===path.resolve(process.argv[1])) {
 const args=process.argv.slice(2);const fixture=generateSchemaFixture(args.find(arg=>!arg.startsWith('--')));const text=JSON.stringify(fixture,null,2)+'\n';
 const target=new URL('../../crates/mcp/tests/fixtures/mcp_oauth_schemas_2_1_322.json',import.meta.url);
 if(args.includes('--write'))writeFileSync(target,text);else assert.equal(hash(readFileSync(target)),hash(text),'native OAuth schema fixture changed');
 console.log(`Claude Code2.1.322 native OAuth schemas: ${fixture.cases.length} cases verified`);
}
