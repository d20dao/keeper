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

/// AirnodeHub passthrough: the provider's own request, sent to the gateway's /api followed by the provider's path. The
/// gateway attests in X-Airnode-* headers over the response body exactly as received, or over the projected object as
/// compact JSON. Its request hash is keccak256 of the UTF-8 JSON of ["passthrough", method, path, query entries sorted by
/// name, body as sent], with the projection entries sorted by alias appended as a sixth element. A passthrough recipe
/// registers that JSON as both its canonical request and its body.
export interface PassthroughRequest { method: "GET" | "POST"; path: string; query?: Record<string, string>; body?: string; projection?: Record<string, string>; }
export const PASSTHROUGH = "passthrough";
// Path segments, query names and projection aliases are unreserved characters, so they reach the gateway unambiguously.
const UNRESERVED = /^[A-Za-z0-9._~-]+$/;
const sortedEntries = (entries: Record<string, string>) => Object.entries(entries).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0);
export function canonicalPassthroughRequest(request: PassthroughRequest): string {
  const parts: unknown[] = [PASSTHROUGH, request.method, request.path, sortedEntries(request.query ?? {}), request.body ?? ""];
  if (request.projection !== undefined) parts.push(sortedEntries(request.projection));
  const text = JSON.stringify(parts);
  parsePassthroughRequest(text);
  return text;
}
function passthroughEntries(value: unknown, what: string): Array<[string, string]> {
  if (!Array.isArray(value)) throw new Error(`A passthrough ${what} is an array of [name, value] entries`);
  return value.map((entry: unknown, index) => {
    if (!Array.isArray(entry) || entry.length !== 2 || typeof entry[0] !== "string" || typeof entry[1] !== "string" || !UNRESERVED.test(entry[0]))
      throw new Error(`Passthrough ${what} entry ${index} is not a [name, value] pair with an unreserved name`);
    if (index > 0 && !(value[index - 1][0] < entry[0])) throw new Error(`Passthrough ${what} entries must be sorted by name without repeats`);
    return [entry[0], entry[1]];
  });
}
/// The request a passthrough recipe's canonical request describes. Throws unless the text is exactly the compact JSON the
/// gateway hashes and names one request a keeper sends unambiguously: GET without a body or POST with a JSON body, a path
/// of unreserved segments, query names outside the gateway's own x-airnode- parameters, and projection JSON Pointers.
export function parsePassthroughRequest(text: string): PassthroughRequest {
  const parts = JSON.parse(text) as unknown;
  if (!Array.isArray(parts) || (parts.length !== 5 && parts.length !== 6) || parts[0] !== PASSTHROUGH || JSON.stringify(parts) !== text)
    throw new Error('A passthrough request is the compact JSON array ["passthrough", method, path, query, body(, projection)]');
  const [, method, path, query, body, projection] = parts as unknown[];
  if (method !== "GET" && method !== "POST") throw new Error("A passthrough method is GET or POST");
  if (typeof path !== "string" || !path.startsWith("/") || path.slice(1).split("/").some(segment => !UNRESERVED.test(segment) || segment === "." || segment === ".."))
    throw new Error("A passthrough path is / followed by segments of unreserved characters");
  const queryEntries = passthroughEntries(query, "query");
  if (queryEntries.some(([name]) => name.toLowerCase().startsWith("x-airnode-"))) throw new Error("x-airnode- query parameters belong to the gateway");
  if (typeof body !== "string") throw new Error("A passthrough body is a string");
  if (method === "GET" && body !== "") throw new Error("A passthrough GET has no body");
  if (method === "POST") { try { JSON.parse(body); } catch { throw new Error("A passthrough POST body is JSON text"); } }
  const request: PassthroughRequest = { method, path };
  if (queryEntries.length) request.query = Object.fromEntries(queryEntries);
  if (body !== "") request.body = body;
  if (parts.length === 6) {
    const projectionEntries = passthroughEntries(projection, "projection");
    if (!projectionEntries.length || projectionEntries.some(([, pointer]) => !/^\/(?:[^~]|~[01])*$/.test(pointer)))
      throw new Error("A passthrough projection lists alias and JSON Pointer entries");
    request.projection = Object.fromEntries(projectionEntries);
  }
  return request;
}
const percentEncode = (text: string) => Array.from(new TextEncoder().encode(text), byte =>
  UNRESERVED.test(String.fromCharCode(byte)) ? String.fromCharCode(byte) : `%${byte.toString(16).toUpperCase().padStart(2, "0")}`).join("");
/// Where a keeper sends a passthrough request: the gateway's /api and the path, then the query entries and one
/// x-airnode-project parameter per projection entry, all in canonical order, with names and values percent-encoded except
/// unreserved characters. The gateway decodes them before hashing; projected fields come back in the order sent.
export function passthroughUrl(gateway: string, request: PassthroughRequest): string {
  const params = [...sortedEntries(request.query ?? {}).map(([name, value]) => `${percentEncode(name)}=${percentEncode(value)}`),
    ...sortedEntries(request.projection ?? {}).map(([alias, pointer]) => `x-airnode-project=${percentEncode(`${alias}:${pointer}`)}`)];
  return `${gateway.replace(/\/+$/, "")}/api${request.path}${params.length ? `?${params.join("&")}` : ""}`;
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
