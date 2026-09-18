import { AbiCoder, getBytes, hexlify, keccak256, solidityPacked } from "ethers";
export interface ApiRequest { operation: string; parameters: Record<string, unknown>; responseProjection?: Record<string, string>; }
export interface ApiAttestation { timestamp: bigint; data: string; signature: string; }
const abi = AbiCoder.defaultAbiCoder();

// AirnodeHub request canonicalization: every object, at any depth, becomes its [key, value] entries sorted by key,
// and arrays keep their order. The request hash is keccak256 of the UTF-8 JSON of [operation, parameters(, projection)].
function canonical(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonical);
  if (value !== null && typeof value === "object")
    return Object.entries(value).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0).map(([key, v]) => [key, canonical(v)]);
  return value;
}
export function canonicalApiRequest(request: ApiRequest): string {
  const parts = [request.operation, canonical(request.parameters)];
  if (request.responseProjection !== undefined) parts.push(canonical(request.responseProjection));
  return JSON.stringify(parts);
}
export function attestationDigest(requestHash: string, a: ApiAttestation): string {
  return keccak256(solidityPacked(["bytes32", "uint256", "bytes"], [requestHash, a.timestamp, a.data]));
}
export function hashAttestation(requestHash: string, a: ApiAttestation): string {
  return keccak256(abi.encode(["bytes32", "uint256", "bytes32", "bytes32"],
    [requestHash, a.timestamp, keccak256(a.data), keccak256(a.signature)]));
}
export function validateApiSignatureEncoding(signature: string): void {
  const bytes = getBytes(signature);
  if (bytes.length !== 65 || (bytes[64] !== 27 && bytes[64] !== 28) ||
    BigInt(hexlify(bytes.slice(32, 64))) > 0x7fffffffffffffffffffffffffffffff5d576e7357a4501ddfe92f46681b20a0n)
    throw new Error("Expected canonical 65-byte low-s EIP-191 signature");
}
