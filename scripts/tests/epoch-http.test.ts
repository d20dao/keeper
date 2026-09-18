import {test} from "node:test";
import assert from "node:assert/strict";
import {collectEpochHttp} from "../lib/epoch-http.ts";
const profiles=Array.from({length:6},(_,i)=>({endpoint:`https://fixture.invalid/${i}`,body:JSON.stringify({operation:"fixed",parameters:{id:i}})}));
test("bounds concurrency, spaces starts and preserves source order",async()=>{
  let active=0,max=0;const starts:number[]=[];
  const fetcher:typeof fetch=async(input)=>{starts.push(Date.now());max=Math.max(max,++active);await new Promise(r=>setTimeout(r,35));active--;
    return new Response(JSON.stringify({endpoint:String(input)}));};
  const result=await collectEpochHttp(profiles,{fetcher,intervalMs:15});
  assert.equal(max,2);assert.deepEqual(result.map(r=>r.profile),profiles);assert.ok(result.every(r=>r.result.ok));
  for(let i=1;i<starts.length;i++)assert.ok(starts[i]-starts[i-1]>=12);
});
test("429 respects Retry-After and retries identical query only",async()=>{
  const bodies:unknown[]=[];const starts:number[]=[];
  const fetcher:typeof fetch=async(_,init)=>{bodies.push(init!.body);starts.push(Date.now());
    return bodies.length===1?new Response("limited",{status:429,headers:{"Retry-After":"1"}}):new Response("{}");};
  const result=await collectEpochHttp(profiles.slice(0,1),{fetcher,intervalMs:0});
  assert.ok(result[0].result.ok);assert.equal(bodies.length,2);assert.equal(bodies[0],bodies[1]);assert.ok(starts[1]-starts[0]>=990);
});
test("long rate limit and paid response fail closed without another attempt",async()=>{
  for(const status of [429,402]){let calls=0;const fetcher:typeof fetch=async()=>{calls++;return new Response("",{status,headers:{"Retry-After":"30"}});};
    const result=await collectEpochHttp(profiles.slice(0,1),{fetcher,intervalMs:0});assert.equal(calls,1);assert.equal(result[0].result.ok,false);}
});
test("retry count and signed-envelope size are bounded",async()=>{
  let calls=0;const limited:typeof fetch=async()=>{calls++;return new Response("",{status:429,headers:{"Retry-After":"0"}});};
  const failed=await collectEpochHttp(profiles.slice(0,1),{fetcher:limited,intervalMs:0});assert.equal(calls,2);assert.equal(failed[0].result.ok,false);
  const oversized=await collectEpochHttp(profiles.slice(0,1),{fetcher:async()=>new Response("x".repeat(16385))});assert.equal(oversized[0].result.ok,false);
});
