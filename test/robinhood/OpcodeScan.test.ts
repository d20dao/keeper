import {expect} from "chai";
import {BLOCKHASH,BLOCK_OPCODES,CODE_SIZE_LIMIT,COINBASE,FORBIDDEN_BYTES,GASLIMIT,INIT_CODE_SIZE_LIMIT,NUMBER,PREVRANDAO,blockOpcodes,codeConstants,
  compiled,opcodeCounts,runtimeHex} from "../helpers/robinhood.ts";

const ALL={NUMBER:1,BLOCKHASH:1,COINBASE:1,PREVRANDAO:1,GASLIMIT:1};
const NONE={NUMBER:0,BLOCKHASH:0,COINBASE:0,PREVRANDAO:0,GASLIMIT:0};
/// Runtime code with the CBOR metadata tail the scan expects: a one-entry map and its 3-byte length.
const withTail=(code:number[])=>"0x"+Buffer.from([...code,0xa1,0x01,0x02,0x00,0x03]).toString("hex");
/// Every contract a Robinhood deployment runs: the round coordinator, its two helpers, the beacon verifier and the proxy.
const DEPLOYED=[
  ["robinhood/D20VRFCoordinatorRobinhood.sol","D20VRFCoordinatorRobinhood"],
  ["robinhood/D20VRFProofVerifier.sol","D20VRFProofVerifier"],
  ["robinhood/LinkedRandomnessMapping.sol","LinkedRandomnessMapping"],
  ["D20BeaconVerifier.sol","D20BeaconVerifier"],
  ["D20Proxy.sol","D20Proxy"],
] as const;

describe("The scan for block opcodes",()=>{
  it("finds every one of them in a contract compiled to read them, as solc names them",()=>{
    const probe=blockOpcodes(compiled("robinhood/test/BlockEnvironmentProbe.sol").deployedBytecode);
    for(const [name,count] of Object.entries(probe))expect(count,name).to.be.at.least(1);
    // The scan names the opcodes of block.number, blockhash(), block.coinbase, block.prevrandao and block.gaslimit.
    expect(BLOCK_OPCODES).to.deep.equal({NUMBER:0x43,BLOCKHASH:0x40,COINBASE:0x41,PREVRANDAO:0x44,GASLIMIT:0x45});
  });

  it("counts an opcode where it is an instruction, and skips PUSH data and the metadata tail",()=>{
    expect(blockOpcodes(withTail([0x00]))).to.deep.equal(NONE);
    expect(blockOpcodes(withTail([BLOCKHASH,COINBASE,NUMBER,PREVRANDAO,GASLIMIT]))).to.deep.equal(ALL);
    expect(blockOpcodes(withTail([NUMBER,0x5f,NUMBER,NUMBER]))).to.deep.equal({...NONE,NUMBER:3});
    // The same bytes as the data of PUSH1 to PUSH32 are no instruction: PUSH2 swallows two bytes, PUSH5 five.
    expect(blockOpcodes(withTail([0x61,COINBASE,PREVRANDAO]))).to.deep.equal(NONE);
    expect(blockOpcodes(withTail([0x64,BLOCKHASH,COINBASE,NUMBER,PREVRANDAO,GASLIMIT]))).to.deep.equal(NONE);
    expect(blockOpcodes(withTail([0x7f,...Array(32).fill(GASLIMIT),NUMBER]))).to.deep.equal({...NONE,NUMBER:1});
    // Constants appended after the code and read with CODECOPY are data: PUSH1 32, PUSH2 8, PUSH0, CODECOPY, STOP, then one word.
    const copy=[0x60,0x20,0x61,0x00,0x08,0x5f,0x39,0x00];
    expect(blockOpcodes(withTail([...copy,...Array(32).fill(NUMBER)]))).to.deep.equal(NONE);
    expect(blockOpcodes(withTail([...copy.slice(0,7),NUMBER,...Array(32).fill(NUMBER)]))).to.deep.equal({...NONE,NUMBER:1});
    expect(()=>blockOpcodes(withTail([0x60,0x20,0x61,0x00,0x08,0x5f,0x39,0x62,0x00,0x00,0x00,...Array(32).fill(NUMBER)]))).to.throw("inside an instruction");
    // Bytes in the CBOR tail are not code either, and a tail is required.
    expect(blockOpcodes("0x"+Buffer.from([0x00,0xa1,NUMBER,COINBASE,0x00,0x03]).toString("hex"))).to.deep.equal(NONE);
    expect(()=>opcodeCounts("0x"+Buffer.from([NUMBER,NUMBER]).toString("hex"))).to.throw("no CBOR metadata tail");
  });
});

describe("Robinhood runtime code",()=>{
  it("reads no block number, block hash, coinbase, prevrandao or gas limit",()=>{
    for(const [source,name] of DEPLOYED)expect(blockOpcodes(compiled(source,name).deployedBytecode),name).to.deep.equal(NONE);
    // The coordinator's appended constants are the storage namespaces it reads, BeaconBook's among them: data, not code.
    const constants=codeConstants(compiled("robinhood/D20VRFCoordinatorRobinhood.sol").deployedBytecode);
    expect(constants).to.include("0xf6225eefaeaf4ae83c5e55b6cf54b47540db50cae2f6ec243c42137121953f00");
    expect(constants).to.include("0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc");
  });

  it("contains neither the arbBlockHash selector nor the EIP-2935 history address",()=>{
    // The positive control: the byte search finds what is there.
    const coordinator=runtimeHex(compiled("robinhood/D20VRFCoordinatorRobinhood.sol"));
    expect(coordinator).to.include("a3b1b31d","the arbBlockNumber() selector ArbitrumBlocks calls");
    for(const [source,name] of DEPLOYED){
      const code=runtimeHex(compiled(source,name));
      for(const [label,bytes] of Object.entries(FORBIDDEN_BYTES))expect(code.includes(bytes),`${name}: ${label}`).to.equal(false);
    }
  });

  it("fits EIP-170 and EIP-3860: the coordinator and its helpers deploy on a chain that enforces the limits",()=>{
    const sizes=DEPLOYED.slice(0,3).map(([source,name])=>{
      const artifact=compiled(source,name);
      return {name,runtime:(artifact.deployedBytecode.length-2)/2,init:(artifact.bytecode.length-2)/2};
    });
    for(const {name,runtime,init} of sizes){
      expect(runtime,`${name} runtime`).to.be.at.most(CODE_SIZE_LIMIT);
      expect(init,`${name} init code`).to.be.at.most(INIT_CODE_SIZE_LIMIT);
    }
    console.log(`Robinhood code sizes (limit ${CODE_SIZE_LIMIT}): ${sizes.map(s=>`${s.name} runtime ${s.runtime} (margin ${CODE_SIZE_LIMIT-s.runtime}), init ${s.init}`).join("; ")}`);
  });

  it("links the mapping library and calls the proof verifier instead of carrying them",()=>{
    const artifact=compiled("robinhood/D20VRFCoordinatorRobinhood.sol");
    expect(Object.keys(artifact.linkReferences)).to.deep.equal(["project/contracts/robinhood/LinkedRandomnessMapping.sol"]);
    expect(Object.keys(artifact.linkReferences["project/contracts/robinhood/LinkedRandomnessMapping.sol"])).to.deep.equal(["LinkedRandomnessMapping"]);
    // The proof verifier's address is an immutable next to UUPSUpgradeable's own address.
    expect(Object.keys(artifact.immutableReferences)).to.have.length(2);
  });
});
