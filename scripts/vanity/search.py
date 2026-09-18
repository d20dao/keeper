"""Bounded CREATE2 salt search. Inputs are public; no wallet/key/environment loader."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import time

import numpy as np
import pyopencl as cl

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent


def hex_bytes(value, length):
    if not isinstance(value, str) or not re.fullmatch(r"0x[0-9a-fA-F]{%d}" % (length * 2), value):
        raise ValueError("Invalid public hex input")
    return bytes.fromhex(value[2:])


def address(factory, salt, init_hash):
    script = "const {getCreate2Address}=require('ethers');const a=JSON.parse(process.argv[1]);process.stdout.write(getCreate2Address(...a));"
    return subprocess.check_output(
        ["node", "-e", script, json.dumps([factory, salt, init_hash])], cwd=ROOT, text=True
    ).strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("plan", type=Path)
    parser.add_argument("--seconds", type=float, default=30)
    parser.add_argument("--start", type=int, default=0)
    parser.add_argument("--batch", type=int, default=2**20)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.output.exists():
        raise ValueError("Output already exists; choose a new result path")
    if not 0 < args.seconds <= 3600 or not 256 <= args.batch <= 2**24 or args.batch % 256:
        raise ValueError("Invalid bounded search budget")
    if not 0 <= args.start < 2**64 - args.batch:
        raise ValueError("Invalid start nonce")
    raw = args.plan.read_bytes()
    plan = json.loads(raw)
    factory = hex_bytes(plan["factory"], 20)
    init_hash = hex_bytes(plan["initCodeHash"], 32)
    salt_prefix = hex_bytes(plan.get("saltPrefix", "0x" + "00" * 24), 24)
    prefix = plan["prefix"].removeprefix("0x").lower()
    if not re.fullmatch(r"[0-9a-f]{1,40}", prefix):
        raise ValueError("Invalid address prefix")
    devices = [d for p in cl.get_platforms() for d in p.get_devices(device_type=cl.device_type.GPU)]
    if not devices:
        raise RuntimeError("No OpenCL GPU available")
    device = next((d for d in devices if "NVIDIA" in d.vendor.upper()), devices[0])
    context = cl.Context([device])
    queue = cl.CommandQueue(context)
    # The upstream optimized permutation computes exactly digest bytes12..31.
    # Keep its complete source unchanged in vendor; compile only its permutation.
    upstream = (HERE / "vendor/keccak256.cl").read_text()
    permutation = upstream.split("#define hasTotal(d)", 1)[0]
    message = b"\xff" + factory + salt_prefix + bytes(8) + init_hash
    initial = "\n".join(f"b[{i}]={v};" for i, v in enumerate(message))
    compare = " && ".join(f"((b[{12+i//2}] >> {4 if i % 2 == 0 else 0}) & 15)=={int(c,16)}" for i, c in enumerate(prefix))
    kernel = permutation + f"""
__kernel void search(ulong start, __global uint *out) {{
  ulong state[25];
  for(int i=0;i<25;i++)state[i]=0;
  uchar *b=(uchar*)state;
  {initial}
  ulong nonce=start+get_global_id(0);
  for(int i=0;i<8;i++)b[45+i]=(uchar)(nonce >> (56-i*8));
  b[85]=1; b[135]=128;
  keccakf(state);
  if({compare}) {{
    if(atomic_cmpxchg(out,0u,1u)==0u) {{out[1]=(uint)nonce;out[2]=(uint)(nonce>>32);}}
  }}
}}
__kernel void probe(ulong nonce, __global uchar *out) {{
  ulong state[25];for(int i=0;i<25;i++)state[i]=0;
  uchar *b=(uchar*)state;
  {initial}
  for(int i=0;i<8;i++)b[45+i]=(uchar)(nonce >> (56-i*8));
  b[85]=1; b[135]=128;keccakf(state);
  for(int i=0;i<20;i++)out[i]=b[12+i];
}}
"""
    program = cl.Program(context, kernel).build()
    probe = program.probe
    check = np.zeros(20, dtype=np.uint8)
    check_buffer = cl.Buffer(context, cl.mem_flags.WRITE_ONLY, check.nbytes)
    for nonce in [0, 1, 2**32 + 7, args.start]:
        probe(queue, (1,), None, np.uint64(nonce), check_buffer)
        cl.enqueue_copy(queue, check, check_buffer).wait()
        salt = "0x" + (salt_prefix + nonce.to_bytes(8, "big")).hex()
        expected = address(plan["factory"], salt, plan["initCodeHash"])
        if "0x" + check.tobytes().hex() != expected.lower():
            raise RuntimeError("GPU CREATE2 result failed independent ethers verification")
    output = np.zeros(3, dtype=np.uint32)
    output_buffer = cl.Buffer(context, cl.mem_flags.READ_WRITE | cl.mem_flags.COPY_HOST_PTR, hostbuf=output)
    search = program.search
    start, count, found = time.monotonic(), 0, None
    print(json.dumps({"device": device.name, "prefix": prefix, "selfCheck": True, "secondsBudget": args.seconds}), flush=True)
    while time.monotonic() - start < args.seconds and args.start + count + args.batch < 2**64:
        search(queue, (args.batch,), (256,), np.uint64(args.start + count), output_buffer)
        cl.enqueue_copy(queue, output, output_buffer).wait()
        count += args.batch
        if output[0]:
            nonce = int(output[1]) | (int(output[2]) << 32)
            salt = "0x" + (salt_prefix + nonce.to_bytes(8, "big")).hex()
            result_address = address(plan["factory"], salt, plan["initCodeHash"])
            if not result_address[2:].lower().startswith(prefix):
                raise RuntimeError("GPU candidate failed independent prefix verification")
            found = {"salt": salt, "address": result_address}
            break
    elapsed = time.monotonic() - start
    result = {"planSha256": hashlib.sha256(raw).hexdigest(), "factory": plan["factory"], "initCodeHash": plan["initCodeHash"],
              "prefix": prefix, "saltPrefix": "0x" + salt_prefix.hex(), "device": device.name,
              "start": str(args.start), "nextStart": str(args.start + count), "attempts": str(count),
              "seconds": elapsed, "hashesPerSecond": count / elapsed, "match": found}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("x", encoding="utf8") as file:
        json.dump(result, file, indent=2)
        file.write("\n")
    print(json.dumps(result), flush=True)


if __name__ == "__main__":
    main()
