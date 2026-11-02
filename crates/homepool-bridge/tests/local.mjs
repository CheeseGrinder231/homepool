// Explicit throwaway Garage/workerd integration; invoke with pinned scratch tools.
import { readFile,writeFile,mkdir,mkdtemp,rm,chmod,stat } from "node:fs/promises";
import { pathToFileURL,fileURLToPath } from "node:url";
import { join,resolve } from "node:path";
import { spawn,spawnSync,execFile } from "node:child_process";
import { randomBytes,createHash } from "node:crypto";
import { createServer } from "node:net";
import { createServer as httpServer,request as httpRequest } from "node:http";
import { lookup } from "node:dns/promises";
import { promisify } from "node:util";
const execute=promisify(execFile);
import assert from "node:assert/strict";
import { Session } from "../../../web/protocol/noise.mjs";
import { encode,decode,envelope,unwrap,admission,ack } from "../../../web/protocol/wire.mjs";
const root=fileURLToPath(new URL("../../../",import.meta.url));
const binary=resolve(process.env.CARGO_TARGET_DIR,"debug/homepool-bridge");
const loopback=(await lookup("localhost",{family:4})).address;
const origin=(scheme,address)=>`${scheme}://${address}`;
const delay=ms=>new Promise(r=>setTimeout(r,ms));
async function port(){return new Promise(r=>{const s=createServer();s.listen(0,loopback,()=>{const p=s.address().port;s.close(()=>r(p));});});}
const scratch=await mkdtemp(join(process.env.TMPDIR,"bridge-proof-"));
async function privateFile(path,data){await writeFile(path,typeof data==="string"?data:JSON.stringify(data),{mode:0o600});}
const children=[];
process.on("exit",()=>{for(const p of children)if(p.exitCode===null)p.kill("SIGKILL");});
function child(command,args){const p=spawn(command,args,{stdio:["ignore","ignore","pipe"]});children.push(p);p.stderr.on("data",b=>{if(command===binary)process.stderr.write(b);});return p;}
function run(command,args){const r=spawnSync(command,args,{encoding:"utf8"});assert.equal(r.status,0,r.stderr);return r.stdout;}
async function wait(fn,attempts=200){for(let i=0;i<attempts;i++){try{return await fn();}catch{}await delay(100);}throw Error("local startup timeout");}
const s3Port=await port(),rpcPort=await port(),proxyPort=await port();
let holdPut=false,storeConfirmed=false,rejectPut=false,failAfterPut=false,rejectHeads=false;
let headRequests=0,putRequests=0;
const heldResponses=[];
const proxy=httpServer((req,res)=>{
 if(req.method==="PUT")putRequests++;
 if(req.method==="HEAD")headRequests++;
 if(req.method==="PUT"&&rejectPut||req.method==="HEAD"&&rejectHeads){req.resume();res.writeHead(503,{"content-length":"0"});res.end();return;}
 const upstream=httpRequest({host:loopback,port:s3Port,path:req.url,method:req.method,headers:req.headers},response=>{
  if(holdPut&&req.method==="PUT"){storeConfirmed=true;heldResponses.push(res);response.resume();return;}
  if(failAfterPut&&req.method==="PUT"){response.resume();res.writeHead(503,{"content-length":"0"});res.end();return;}
  res.writeHead(response.statusCode,response.headers);response.pipe(res);
 });
 upstream.on("error",()=>res.destroy());req.pipe(upstream);
});
await new Promise(r=>proxy.listen(proxyPort,loopback,r));
const garage=process.env.HOMEPOOL_TEST_GARAGE;
const config=join(scratch,"garage.toml");
await privateFile(config,`metadata_dir = "${join(scratch,"meta")}"
data_dir = "${join(scratch,"data")}"
db_engine = "sqlite"
replication_factor = 1
rpc_bind_addr = "${loopback}:${rpcPort}"
rpc_public_addr = "${loopback}:${rpcPort}"
rpc_secret = "${randomBytes(32).toString("hex")}"
[s3_api]
s3_region = "garage"
api_bind_addr = "${loopback}:${s3Port}"
root_domain = ".s3.garage.localhost"
`);
const cli=(...args)=>run(garage,["-c",config,...args]);
child(garage,["-c",config,"server"]);
await wait(()=>cli("status"));
const node=cli("node","id","-q").trim().split("@")[0];
cli("layout","assign","-z","zone-a","-c","1G",node);cli("layout","apply","--version","1");
const bucket="homepool-consumer-1";cli("bucket","create",bucket);
const created=cli("key","create","bridge-test");
const access=created.match(/Key ID:\s*(\S+)/)[1],secret=created.match(/Secret key:\s*(\S+)/)[1];
cli("bucket","allow","--read","--write",bucket,"--key",access);
const { Miniflare } = await import(pathToFileURL(process.env.MINIFLARE_MODULE));
const modules = {};
for (const file of [
  "relay/worker.mjs",
  "relay/route.mjs",
  "relay/limits.mjs",
  "relay/budget.mjs",
  "web/protocol/wire.mjs",
  "web/protocol/schema.mjs",
  "web/protocol/noise.mjs",
])
  modules[file] = {
    type: "esm",
    contents: await readFile(new URL("../../../" + file, import.meta.url), "utf8"),
  };
const text = (value) => ({ type: "text", value });
process.chdir(process.env.TMPDIR);
const options={
  workers: [
    {
      config: {
        name: "relay",
        compatibilityDate: "2026-11-01",
        manifest: { mainModule: "relay/worker.mjs", modules },
        env: {
          KILL_SWITCH: text("false"),
          ROUTES: text('["example"]'),
          BRIDGE_TOKENS: text(JSON.stringify({ example: "ab".repeat(32) })),
          ROUTES_DO: {
            type: "durable-object",
            worker: "relay",
            exportName: "Relay",
          },
          UPGRADES: {
            type: "durable-object",
            worker: "relay",
            exportName: "UpgradeGate",
          },
        },
        exports: {
          Relay: { type: "durable-object", storage: "sqlite" },
          UpgradeGate: { type: "durable-object", storage: "sqlite" },
        },
      },
    },
  ],
};
let mf=new Miniflare(options);

try {
 const relay=await mf.ready;
 const stateDir=join(scratch,"state");run(binary,["init",stateDir]);
 const credentials=join(scratch,"credentials.json"),tokenFile=join(scratch,"token"),bridgeConfig=join(scratch,"bridge.json");
 await privateFile(credentials,{version:1,bucket,aws_access_key_id:access,aws_secret_access_key:secret});
 await privateFile(tokenFile,"ab".repeat(32));
 await privateFile(bridgeConfig,{state_dir:stateDir,relay_url:origin("ws",relay.host)+"/example",route:"example",bridge_token_file:tokenFile,page_origin:origin("https","192.0.2.1")+"/",s3_endpoint:origin("http",`${loopback}:${proxyPort}`),bucket,region:"garage",credentials_file:credentials});
 let bridge=child(binary,["run",bridgeConfig]);console.log("local bridge started");
 const command=async(...args)=>(await execute(binary,[...args.slice(0,1),bridgeConfig,...args.slice(1)])).stdout;
 await wait(()=>command("devices"));await delay(300);
 async function phone(mode,invite,key,deviceToken){
  const ws=new WebSocket(origin("ws",relay.host)+"/example");ws.binaryType="arraybuffer";
  const queue=[],waiting=[];let closed=false;ws.addEventListener("message",e=>{const b=new Uint8Array(e.data);if(waiting.length)waiting.shift()(b);else queue.push(b);});
  ws.addEventListener("close",()=>{closed=true;for(const r of waiting.splice(0))r(null);});
  const next=()=>queue.length?Promise.resolve(queue.shift()):closed?Promise.reject(Error("closed")):Promise.race([new Promise(r=>waiting.push(r)),delay(10000).then(()=>{throw Error("receive deadline");})]);
  await new Promise((r,j)=>{ws.onopen=r;ws.onerror=j;});
  ws.send(admission("phone",deviceToken??invite.get("token")));
  const open=unwrap(await next());assert.equal(open.kind,"open");const id=open.session;
  const session=await Session.create({mode,initiator:true,route:"example",privateKey:key.privateKey,publicKey:new Uint8Array(await crypto.subtle.exportKey("raw",key.publicKey)),remoteStatic:new Uint8Array(Buffer.from(invite.get("bridge"),"hex")),psk:mode==="enroll"?new Uint8Array(Buffer.from(invite.get("psk"),"hex")):undefined});
  let received=0,acked=0;async function data(handshake=false){for(;;){const b=await next();if(!b)throw Error("closed");const e=unwrap(b);if(e.kind==="data"){received+=b.length;if(handshake||received-acked>=65536){ws.send(ack(id,BigInt(received)));acked=received;}return e.data;}if(e.kind==="close"||e.kind==="error")throw Error("closed");}}
  ws.send(envelope("data",id,await session.writeHandshake()));await session.readHandshake(await data(true));
  if(mode==="enroll")ws.send(envelope("data",id,await session.writeHandshake()));
  const send=async m=>ws.send(envelope("data",id,await session.encrypt(encode(m))));
  const raw=async bytes=>ws.send(envelope("data",id,await session.encrypt(bytes)));
  const receive=async()=>{const m=decode(await session.decrypt(await data()));if(m.type!=="chunk"&&received!==acked){ws.send(ack(id,BigInt(received)));acked=received;}return m;};
  return {ws,send,receive,raw,closed:()=>closed};
 }
 const invite=async()=>new URLSearchParams(JSON.parse(await command("invite")).link.split("#")[1]);
 const keys=await Promise.all([0,1].map(()=>crypto.subtle.generateKey("X25519",false,["deriveBits"])));
 const invitations=[await invite(),await invite()];await delay(300);
 const enrolled=await Promise.all(keys.map(async(key,i)=>{const p=await phone("enroll",invitations[i],key);const ack=await p.receive();assert.equal(ack.type,"enrolled");p.ws.close();return ack;}));
 await delay(300);
 const invitationReconnect=await phone("reconnect",invitations[0],keys[0]);
 assert.deepEqual(await invitationReconnect.receive(),enrolled[0]);invitationReconnect.ws.close();await delay(250);
 await assert.rejects(phone("reconnect",invitations[0],keys[1]));
 const phones=await Promise.all(keys.map((key,i)=>phone("reconnect",invitations[i],key,enrolled[i].token)));
 console.log("two enrollments and IK reconnects passed");
 // A validly paired device sends records the schema or operation policy refuses.
 const requestRefuse=()=>randomBytes(16).toString("hex");
 const id=requestRefuse;
 const state=async()=>JSON.parse(await readFile(join(stateDir,"state.json"),"utf8"));
 const row=async(device,request)=>(await state()).journal[`${device}:${request}`];
 const spoolPath=(device,request)=>join(stateDir,`${device}-${request}.spool`);
 async function terminal(device,request,status){await wait(async()=>{assert.equal((await row(device,request)).status,status);await assert.rejects(stat(spoolPath(device,request)),{code:"ENOENT"});});}
 async function partial(p,length=50*1024*1024){const request=id();await p.send({type:"put-begin",id:request,name:"partial",mime:"",length});assert.equal((await p.receive()).type,"credit");await p.send({type:"chunk",id:request,index:0,data:new Uint8Array([1])});return request;}
 async function probe(p){
  const request=id();await p.send({type:"put-begin",id:request,name:"probe.jpg",mime:"image/jpeg",length:3});assert.equal((await p.receive()).type,"credit");
  await p.send({type:"chunk",id:request,index:0,data:new Uint8Array([11,22,33])});await p.send({type:"put-end",id:request,length:3,count:1});const done=await p.receive();assert.equal(done.type,"put-done",JSON.stringify(done));
  const get=id();await p.send({type:"get",id:get,key:done.key});assert.equal((await p.receive()).type,"get-head");await p.send({type:"credit",id:get,chunks:1});const chunk=await p.receive();assert.equal(chunk.type,"chunk");assert.deepEqual(Array.from(chunk.data),[11,22,33]);assert.equal((await p.receive()).type,"get-end");
 }

 await phones[0].send({type:"put-begin",id:requestRefuse(),name:"large",mime:"",length:50*1024*1024+1});assert.equal((await phones[0].receive()).type,"error");
 await phones[0].send({type:"enrolled",id:requestRefuse(),token:"00".repeat(32)});assert.equal((await phones[0].receive()).type,"error");
 for(const malformed of [new Uint8Array([1,255]),new Uint8Array([...encode({type:"put-begin",id:requestRefuse(),name:"chosen",mime:"",length:1}),0,1,120])]) {
  await phones[0].raw(malformed);await wait(()=>{assert(phones[0].closed());});await delay(200);phones[0]=await phone("reconnect",invitations[0],keys[0],enrolled[0].token);
 }
 const badGet=encode({type:"get",id:requestRefuse(),key:"2026-11-02/123456-aaaaaaaaaaaaaaaaaaaaaaaaaa-image"});badGet[38]=120;
 await phones[0].raw(badGet);await wait(()=>{assert(phones[0].closed());});await delay(200);phones[0]=await phone("reconnect",invitations[0],keys[0],enrolled[0].token);
 await assert.rejects(phone("enroll",invitations[0],keys[0]));
 console.log("paired oversize, unknown operation, chosen key/overwrite, bad get key and used invitation refusals passed");
 // Review regression: declared capacity matters even with only one byte sent.
 const originalPid=bridge.pid;
 const beforeChurnPuts=putRequests,beforeChurnHeads=headRequests;
 for(let churn=0;churn<4;churn++){
  const request=await partial(phones[0]);phones[0].ws.close();
  await terminal(enrolled[0].id,request,"failed");await delay(200);
  phones[0]=await phone("reconnect",invitations[0],keys[0],enrolled[0].token);
  await phones[0].send({type:"put-status",id:request});assert.equal((await phones[0].receive()).type,"failed");
 }
 assert.equal(putRequests,beforeChurnPuts);assert.equal(headRequests,beforeChurnHeads);
 await probe(phones[1]);console.log("four 50 MiB-declared disconnects settle without PUT/HEAD; other device PUT/GET continues");
 // Exercise the actual 60-second progress deadline, with a full-sized reservation.
 const timedOut=await partial(phones[0]);
 console.log("waiting for actual upload progress timeout");
 await wait(()=>{assert(phones[0].closed());},750);
 await terminal(enrolled[0].id,timedOut,"failed");
 phones[0]=await phone("reconnect",invitations[0],keys[0],enrolled[0].token);
 await phones[0].send({type:"put-status",id:timedOut});assert.equal((await phones[0].receive()).type,"failed");
 await probe(phones[1]);console.log("progress timeout settles; other device PUT/GET continues without restart");
 // An attempted PUT that fails before reaching Garage must be resolved by HEAD.
 rejectPut=true;const rejected=await partial(phones[0],1),headsBeforeError=headRequests;
 await phones[0].send({type:"put-end",id:rejected,length:1,count:1});assert.equal((await phones[0].receive()).type,"error");rejectPut=false;
 assert(headRequests>headsBeforeError);await terminal(enrolled[0].id,rejected,"failed");
 await probe(phones[1]);console.log("store PUT error reconciles to failed; other device PUT/GET continues");
 // A lost session after a real PUT must retain its spool while the worker is
 // blocked and while HEAD fails, then commit when reconciliation can succeed.
 holdPut=true;rejectHeads=true;storeConfirmed=false;
 const detached=await partial(phones[0],1);await phones[0].send({type:"put-end",id:detached,length:1,count:1});
 await wait(()=>{assert(storeConfirmed);});const headsBeforeDetach=headRequests;
 phones[0].ws.close();await wait(()=>{assert(phones[0].closed());});await delay(500);
 assert.equal(headRequests,headsBeforeDetach);assert.equal((await row(enrolled[0].id,detached)).status,"pending");assert((await stat(spoolPath(enrolled[0].id,detached))).size>0);
 holdPut=false;for(const res of heldResponses.splice(0))res.destroy();
 await wait(()=>{assert(headRequests>headsBeforeDetach);});
 assert.equal((await row(enrolled[0].id,detached)).status,"pending");await stat(spoolPath(enrolled[0].id,detached));
 await probe(phones[1]);rejectHeads=false;
 await terminal(enrolled[0].id,detached,"committed");
 phones[0]=await phone("reconnect",invitations[0],keys[0],enrolled[0].token);
 await phones[0].send({type:"put-status",id:detached});assert.equal((await phones[0].receive()).type,"committed");
 console.log("detached PUT worker exits before HEAD; failed HEAD preserves spool; live retry commits");
 // A definite error response can still hide a successfully stored object.
 failAfterPut=true;const hidden=await partial(phones[0],1);await phones[0].send({type:"put-end",id:hidden,length:1,count:1});assert.equal((await phones[0].receive()).type,"put-done");failAfterPut=false;
 await terminal(enrolled[0].id,hidden,"committed");await probe(phones[1]);
 // Revoke a third paired device with a 50 MiB reservation; A/B keep their keys.
 const thirdInvite=await invite(),thirdKey=await crypto.subtle.generateKey("X25519",false,["deriveBits"]);await delay(200);
 const third=await phone("enroll",thirdInvite,thirdKey);const thirdAck=await third.receive();const revoked=await partial(third);
 await command("revoke",thirdAck.id);await wait(()=>{assert(third.closed());});await terminal(thirdAck.id,revoked,"failed");
 await probe(phones[1]);assert.equal(bridge.pid,originalPid);
 console.log("revoked upload releases capacity; other device PUT/GET continues; bridge PID unchanged");
 const bytes=new Uint8Array(10*1024*1024);for(let i=0;i<bytes.length;i++)bytes[i]=i%251;

 async function upload(p){const request=id();await p.send({type:"put-begin",id:request,name:"image.jpg",mime:"image/jpeg",length:bytes.length});let at=0,index=0;while(at<bytes.length){const credit=await p.receive();assert.equal(credit.type,"credit",JSON.stringify(credit));for(let n=0;n<credit.chunks&&at<bytes.length;n++){const data=bytes.slice(at,at+61440);await p.send({type:"chunk",id:request,index:index++,data});at+=data.length;await delay(40);}}
 await p.send({type:"put-end",id:request,length:bytes.length,count:index});let done=await p.receive();if(done.type==="credit")done=await p.receive();assert.equal(done.type,"put-done",JSON.stringify(done));return done.key;}
 const objects=await Promise.all(phones.map(upload));
 await Promise.all(phones.map(async(p,i)=>{const request=id();await p.send({type:"list",id:request,cursor:"",limit:100});const page=await p.receive();assert.equal(page.type,"list-page",JSON.stringify(page));assert(page.items.some(item=>item.key===objects[0]));assert(page.items.some(item=>item.key===objects[1]));
 const request2=id();await p.send({type:"get",id:request2,key:objects[1-i]});const head=await p.receive();assert.equal(head.type,"get-head",JSON.stringify(head));assert.equal(head.mime,"image/jpeg");let chunks=0;const hash=createHash("sha256");await p.send({type:"credit",id:request2,chunks:8});for(;;){const m=await p.receive();if(m.type==="get-end")break;assert.equal(m.type,"chunk");hash.update(m.data);chunks++;if(chunks%8===0)await p.send({type:"credit",id:request2,chunks:8});}assert.equal(hash.digest("hex"),createHash("sha256").update(bytes).digest("hex"));}));
 // Confirmed store write, withheld response: kill before the journal can commit.
 const uncertain=id();holdPut=true;storeConfirmed=false;
 await phones[0].send({type:"put-begin",id:uncertain,name:"crash.jpg",mime:"image/jpeg",length:1});assert.equal((await phones[0].receive()).type,"credit");
 await phones[0].send({type:"chunk",id:uncertain,index:0,data:new Uint8Array([42])});
 await phones[0].send({type:"put-end",id:uncertain,length:1,count:1});
 await wait(()=>{assert(storeConfirmed);});
 const before=JSON.parse(await readFile(join(stateDir,"state.json"),"utf8"));assert.equal(before.journal[`${enrolled[0].id}:${uncertain}`].status,"pending");
 bridge.kill("SIGKILL");await new Promise(r=>bridge.once("exit",r));holdPut=false;
 bridge=child(binary,["run",bridgeConfig]);await wait(()=>command("devices"));await delay(1500);
 const recovered=await phone("reconnect",invitations[0],keys[0],enrolled[0].token);
 await recovered.send({type:"put-status",id:uncertain});const status=await recovered.receive();assert.equal(status.type,"committed");assert.equal(status.length,1);
 console.log("kill after confirmed store PUT, restart reconciliation and device-scoped committed status passed");
 phones[0]=recovered;
 // Replace the relay runtime on the same loopback port, with a fresh token registry.
 const relayPort=Number(relay.port);await mf.dispose();mf=new Miniflare({...options,port:relayPort});await mf.ready;
 await delay(4000);
 const afterRelay=await phone("reconnect",invitations[0],keys[0],enrolled[0].token);phones[0]=afterRelay;
 const pingId=id();await afterRelay.send({type:"ping",id:pingId});assert.equal((await afterRelay.receive()).type,"pong");
 console.log("relay replacement, bridge reconnect and token re-sync passed");
 await command("revoke",enrolled[0].id);await wait(()=>{assert(phones[0].closed());});
 console.log("two enrollments, IK reconnects, interleaved 10 MiB PUTs, list, checksum GETs, live revoke passed");
 for(const p of phones)p.ws.close();
} finally {await mf.dispose();proxy.closeAllConnections();proxy.close();for(const p of children)p.kill("SIGKILL");await rm(scratch,{recursive:true,force:true});}
