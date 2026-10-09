// Public computation only, safe to bundle for browsers. The rules of D20VRFCoordinatorRobinhood and its BeaconBook, recomputed off
// chain: which drand round a request binds, the beacon schedule from its events, a beacon's identity, a request's seed, transcript
// and configuration hash, and a replay of one fulfilled request. This is mathematical replay only: that the events, receipts and
// timestamps it is given are the chain's canonical ones, and which implementation ran at each block, are for a separate chain check.
import { AbiCoder, Interface, ZeroHash, getAddress, getBytes, hexlify, id, keccak256, type BytesLike } from "ethers";
import { drandRandomness, verifyDrandRound } from "./drand.ts";
import { hashMapping, mapRandomness, type MappingSpec } from "./mapping.ts";
import { hashProof, hashPublicKey, verifyVRFProof, type VRFProof, type XY } from "./vrf.ts";

/// The coordinator's domains: SEED_DOMAIN, TRANSCRIPT_DOMAIN and CONFIG_DOMAIN, and BeaconBook's ROUND_BEACON_DOMAIN.
export const ROUND_SEED_DOMAIN = id("D20_VRF_ROUND_SEED");
export const ROUND_TRANSCRIPT_DOMAIN = id("D20_VRF_ROUND_TRANSCRIPT");
export const ROUND_CONFIG_DOMAIN = id("D20_VRF_ROUND_CONFIG");
export const ROUND_BEACON_DOMAIN = id("D20_ROUND_BEACON_V1");
/// A request binds the first round scheduled at least this many seconds after its block's timestamp (BeaconBook.ROUND_LEAD).
export const ROUND_LEAD = 3n;
/// A request's deadline is its block's timestamp plus this (the coordinator's RESPONSE_TIMEOUT).
export const ROUND_RESPONSE_TIMEOUT = 60n;
/// A scheduled beacon change takes effect at least this long after the block that schedules it (BeaconBook.MIN_SCHEDULE_LEAD).
export const ROUND_MIN_SCHEDULE_LEAD = 600n;
/// The longest round period a beacon may have, and the most beacons there can be (BeaconBook.MAX_BEACON_PERIOD and MAX_BEACONS).
export const ROUND_MAX_BEACON_PERIOD = 10n;
export const ROUND_MAX_BEACONS = 256;

/// A beacon as BeaconBook registers it (getBeacon): its rounds are scheduled at genesis + (r - 1) × period, and verifier checks a round's
/// signature under publicKey. chainHash names the drand network.
export interface RoundBeacon { verifier: string; chainHash: string; publicKey: string; genesis: bigint; period: bigint; }
/// A registered beacon with the identity its BeaconRegistered event carries.
export interface RegisteredRoundBeacon extends RoundBeacon { identity: string; }

const abi = AbiCoder.defaultAbiCoder();
const MAX_UINT64 = (1n << 64n) - 1n, MAX_UINT40 = (1n << 40n) - 1n;
const sameHex = (a: BytesLike, b: BytesLike) => hexlify(a) === hexlify(b);
function uint(value: bigint, max: bigint, what: string): bigint {
  if (typeof value !== "bigint" || value < 0n || value > max) throw new Error(`Invalid ${what}`);
  return value;
}

// ---- assignment

/// When a beacon's round is scheduled: genesis + (round - 1) × period. There is no round 0.
export function roundTime(beacon: Pick<RoundBeacon, "genesis" | "period">, round: bigint): bigint {
  if (uint(round, MAX_UINT64, "round") === 0n) throw new Error("Invalid round");
  return beacon.genesis + (round - 1n) * beacon.period;
}
/// The round a request in a block with this timestamp binds: the first round r with genesis + (r - 1) × period ≥ timestamp + roundLead,
/// and round 1 when that time is at or before genesis.
///
/// The timestamp is the block's as the chain records it. A transaction sent through the parent chain's delayed inbox takes the later of
/// its enqueue time and the previous block's timestamp, which after a quiet period can be seconds or minutes old: such a request can bind
/// a round that was already public when it was included. Nothing in the request's fields shows this; only the transaction's origin does.
export function roundFor(beacon: Pick<RoundBeacon, "genesis" | "period">, timestamp: bigint, roundLead: bigint = ROUND_LEAD): bigint {
  const t = uint(timestamp, MAX_UINT64, "timestamp") + uint(roundLead, MAX_UINT64, "round lead");
  const { genesis, period } = beacon;
  if (typeof period !== "bigint" || period < 1n || typeof genesis !== "bigint" || genesis < 0n) throw new Error("Invalid beacon schedule");
  return t <= genesis ? 1n : (t - genesis + period - 1n) / period + 1n;
}

// ---- identity

/// A registration's identity, BeaconBook.beaconIdentity: keccak256(abi.encode(ROUND_BEACON_DOMAIN, verifier, chainHash, keccak256(publicKey),
/// genesis, period)), with genesis and period as uint64. No two registrations share one.
export function roundBeaconIdentity(beacon: RoundBeacon): string {
  return keccak256(abi.encode(["bytes32", "address", "bytes32", "bytes32", "uint64", "uint64"],
    [ROUND_BEACON_DOMAIN, beacon.verifier, beacon.chainHash, keccak256(beacon.publicKey), beacon.genesis, beacon.period]));
}

// ---- the schedule from events

/// A log as eth_getLogs returns it, with the timestamp of its block, which the caller reads from the block's header.
export interface RoundBeaconLog {
  address: string; topics: readonly string[]; data: string;
  blockNumber: bigint | number | string; logIndex?: bigint | number | string; index?: bigint | number | string;
  blockTimestamp: bigint | number | string; removed?: boolean;
}
/// A beacon in force for requests whose block timestamp is `since` or later, until the next era.
export interface RoundEra { beaconId: number; since: bigint; }
/// The beacons, as registered, and the schedule: the eras in force so far, oldest first, and a change still pending, which takes
/// effect at fromTime without another event.
export interface RoundSchedule {
  beacons: readonly RegisteredRoundBeacon[];
  eras: readonly RoundEra[];
  pending?: { beaconId: number; fromTime: bigint };
}

let beaconEvents: Interface | undefined;
const events = () => beaconEvents ??= new Interface([
  "event BeaconRegistered(uint8 indexed beaconId, bytes32 indexed identity, address indexed verifier, bytes32 chainHash, bytes publicKey, uint64 genesis, uint64 period)",
  "event BeaconScheduled(uint8 indexed beaconId, uint64 indexed fromTime)",
  "event BeaconScheduleCancelled(uint8 indexed beaconId, uint64 indexed fromTime)",
]);
/// A block number, log index or timestamp: a non-negative integer as a bigint, a safe number, or a decimal or 0x-hex string.
function position(value: bigint | number | string | undefined, what: string): bigint {
  const valid = typeof value === "string" ? /^(0|[1-9][0-9]*|0x[0-9a-fA-F]+)$/.test(value) : typeof value === "bigint" ? value >= 0n
    : typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
  if (!valid) throw new Error(`Invalid beacon log ${what}`);
  return BigInt(value as bigint | number | string);
}
/// The decoded event of a log, which must be exactly the encoding of its own values: no extra topic or data and no stray bits.
function decode(log: RoundBeaconLog) {
  const parsed = events().parseLog({ topics: [...log.topics], data: log.data });
  if (parsed === null) throw new Error("Invalid beacon event");
  const again = events().encodeEventLog(parsed.fragment, Array.from(parsed.args));
  if (again.data !== log.data.toLowerCase() || again.topics.join() !== log.topics.join().toLowerCase()) throw new Error("Noncanonical beacon event");
  return parsed;
}

/// The beacon schedule of a coordinator from its BeaconRegistered, BeaconScheduled and BeaconScheduleCancelled logs, which must run from
/// its initialization. It applies BeaconBook's rules:
/// - registrations are appended, so the n-th has id n, and carry their own identity;
/// - the first BeaconScheduled is initialization's: beacon 0, in force from that block's timestamp;
/// - every later one starts at least ROUND_MIN_SCHEDULE_LEAD after its block and replaces the pending change. A pending change whose time
///   had come by that block took effect: it becomes an era and stays one. A change that had not is replaced, and the contract emits no
///   BeaconScheduleCancelled for it;
/// - BeaconScheduleCancelled drops the pending change before it takes effect.
/// Logs of other events are ignored. A log from another address, a removed or repeated log, a noncanonical encoding, or a sequence the
/// contract would have refused throws.
export function roundScheduleFromEvents(coordinator: string, logs: readonly RoundBeaconLog[]): RoundSchedule {
  const address = getAddress(coordinator), topics = new Set(["BeaconRegistered", "BeaconScheduled", "BeaconScheduleCancelled"]
    .map(name => events().getEvent(name)!.topicHash));
  const ordered = logs.flatMap(log => {
    if (!topics.has(log.topics[0]?.toLowerCase())) return [];
    if (getAddress(log.address) !== address) throw new Error("A beacon event is from another address");
    if (log.removed) throw new Error("A beacon log was removed");
    return [{ log, block: position(log.blockNumber, "block number"), index: position(log.logIndex ?? log.index, "log index"),
      time: position(log.blockTimestamp, "block timestamp") }];
  }).sort((a, b) => a.block !== b.block ? (a.block < b.block ? -1 : 1) : a.index < b.index ? -1 : a.index > b.index ? 1 : 0);
  const beacons: RegisteredRoundBeacon[] = [], eras: RoundEra[] = [];
  let pending: RoundSchedule["pending"];
  ordered.forEach(({ log, block, index, time }, at) => {
    const previous = ordered[at - 1];
    if (previous && previous.block === block && previous.index === index) throw new Error("A beacon log is repeated");
    if (previous && (previous.block === block ? previous.time !== time : previous.time > time)) throw new Error("Beacon log timestamps go back");
    const { name, args } = decode(log);
    const beaconId = Number(args.beaconId);
    if (name === "BeaconRegistered") {
      const beacon: RegisteredRoundBeacon = { verifier: getAddress(args.verifier), chainHash: args.chainHash, publicKey: hexlify(args.publicKey),
        genesis: BigInt(args.genesis), period: BigInt(args.period), identity: args.identity };
      if (beaconId !== beacons.length || beacon.chainHash === ZeroHash || beacon.genesis < 1n || beacon.genesis > MAX_UINT40 ||
          beacon.period < 1n || beacon.period > ROUND_MAX_BEACON_PERIOD || roundBeaconIdentity(beacon) !== beacon.identity ||
          beacons.some(known => known.identity === beacon.identity)) throw new Error("Invalid beacon registration");
      beacons.push(beacon);
      return;
    }
    const fromTime = BigInt(args.fromTime);
    if (beaconId >= beacons.length) throw new Error("A beacon schedule names an unregistered beacon");
    if (name === "BeaconScheduled") {
      if (eras.length === 0) {
        if (beaconId !== 0 || fromTime !== time) throw new Error("Invalid first beacon schedule");
        eras.push({ beaconId, since: fromTime });
        return;
      }
      if (fromTime < time + ROUND_MIN_SCHEDULE_LEAD || fromTime > MAX_UINT40) throw new Error("Invalid beacon schedule");
      if (pending && time >= pending.fromTime) eras.push({ beaconId: pending.beaconId, since: pending.fromTime });
      pending = { beaconId, fromTime };
      return;
    }
    if (!pending || pending.beaconId !== beaconId || pending.fromTime !== fromTime || time >= fromTime) throw new Error("Invalid beacon schedule cancellation");
    pending = undefined;
  });
  if (eras.length === 0) throw new Error("The beacon logs do not reach initialization");
  return { beacons, eras, pending };
}

/// The beacon and round that a request in a block with this timestamp binds, as BeaconBook.roundAt answers for any timestamp: the pending
/// change from its time on, else the latest era begun by then, else the first era.
export function roundAt(schedule: RoundSchedule, timestamp: bigint): { beaconId: number; round: bigint; beacon: RegisteredRoundBeacon } {
  const { eras, pending } = schedule;
  let era = eras.length - 1;
  while (era > 0 && timestamp < eras[era].since) era--;
  const beaconId = pending && timestamp >= pending.fromTime ? pending.beaconId : eras[era].beaconId;
  const beacon = schedule.beacons[beaconId];
  return { beaconId, round: roundFor(beacon, timestamp), beacon };
}

// ---- the round

/// A round's randomness, as the coordinator caches it and drand publishes it: sha256 of the round's signature.
export function roundRandomness(signature: BytesLike): string {
  return drandRandomness(signature);
}
/// Whether signature (64 bytes, x ‖ y) is the signature of round under a drand bls-bn254-unchained-on-g1 group key (128 bytes,
/// x_im ‖ x_re ‖ y_im ‖ y_re), as D20BeaconVerifier checks it. Malformed input is false, never a throw.
export function verifyRoundSignature(publicKey: BytesLike, round: bigint, signature: BytesLike): boolean {
  return verifyDrandRound(publicKey, round, signature);
}

// ---- seed, transcript and configuration

/// Every input of a request's seed, as getRoundRequest and the configuration hold them, with its round's randomness.
export interface RoundSeedInput {
  chainId: bigint; coordinator: string; keyHash: string; requestId: bigint; consumer: string; clientSeed: string; mappingHash: string;
  requestBlock: bigint; beaconId: number; round: bigint; roundRandomness: string;
}
/// The VRF input of a request, the coordinator's _seed: keccak256(abi.encode(SEED_DOMAIN, chainId, coordinator, keyHash, requestId,
/// consumer, clientSeed, mappingHash, requestBlock (uint64), beaconId (uint8), round (uint64), roundRandomness)).
export function roundSeed(input: RoundSeedInput): bigint {
  return BigInt(keccak256(abi.encode(
    ["bytes32", "uint256", "address", "bytes32", "uint256", "address", "bytes32", "bytes32", "uint64", "uint8", "uint64", "bytes32"],
    [ROUND_SEED_DOMAIN, input.chainId, input.coordinator, input.keyHash, input.requestId, input.consumer, input.clientSeed, input.mappingHash,
      input.requestBlock, input.beaconId, input.round, input.roundRandomness])));
}
/// What a fulfilled request's transcript binds.
export interface RoundTranscriptInput {
  chainId: bigint; coordinator: string; requestId: bigint; configurationHash: string; beaconId: number; round: bigint; roundRandomness: string;
  proofHash: string; randomness: string; mappingHash: string;
}
/// A fulfilled request's transcript, the coordinator's _transcriptHash: keccak256(abi.encode(TRANSCRIPT_DOMAIN, chainId, coordinator,
/// requestId, protocolConfigurationHash, beaconId (uint8), round (uint64), roundRandomness, proofHash, randomness, mappingHash)).
export function roundTranscriptHash(input: RoundTranscriptInput): string {
  return keccak256(abi.encode(
    ["bytes32", "uint256", "address", "uint256", "bytes32", "uint8", "uint64", "bytes32", "bytes32", "bytes32", "bytes32"],
    [ROUND_TRANSCRIPT_DOMAIN, input.chainId, input.coordinator, input.requestId, input.configurationHash, input.beaconId, input.round,
      input.roundRandomness, input.proofHash, input.randomness, input.mappingHash]));
}
/// What initialization binds into the protocol configuration hash: the VRF key, the initial fee recipient and minimum fee, and the
/// identity of beacon 0.
export interface RoundConfiguration { publicKey: XY; feeRecipient: string; initialMinFee: bigint; beaconIdentity: string; }
/// protocolConfigurationHash: keccak256(abi.encode(CONFIG_DOMAIN, publicKey (uint256[2]), feeRecipient, initialMinFee, beaconIdentity(0))).
/// Later fee, keeper or beacon changes do not change it.
export function roundConfigurationHash(configuration: RoundConfiguration): string {
  return keccak256(abi.encode(["bytes32", "uint256[2]", "address", "uint256", "bytes32"],
    [ROUND_CONFIG_DOMAIN, configuration.publicKey, configuration.feeRecipient, configuration.initialMinFee, configuration.beaconIdentity]));
}

// ---- acceptance

/// A request's deadline: its block's timestamp plus ROUND_RESPONSE_TIMEOUT.
export function roundDeadline(requestTimestamp: bigint): bigint {
  return uint(requestTimestamp, MAX_UINT64, "timestamp") + ROUND_RESPONSE_TIMEOUT;
}
/// Whether a fulfilment in a block with acceptanceTimestamp is accepted for a request made at requestTimestamp: no earlier than the
/// request and at or before its deadline (the coordinator reverts RequestExpired after it). A transaction still pending is not accepted.
export function roundAcceptedInTime(requestTimestamp: bigint, acceptanceTimestamp: bigint): boolean {
  return acceptanceTimestamp >= requestTimestamp && acceptanceTimestamp <= roundDeadline(requestTimestamp);
}

// ---- replay of one request

/// What a fulfilled request left on chain and what its replay needs: the configuration, the registration of the request's beacon, the
/// request's fixed fields and timestamps, its round's signature, the VRF proof, and what the coordinator recorded.
export interface RoundRequestEvidence {
  chainId: bigint; coordinator: string;
  /// The configuration initialization bound, its hash (protocolConfigurationHash) and the key hash (keyHash) requests carry.
  configuration: RoundConfiguration; configurationHash: string; keyHash: string;
  /// The registration of the request's beacon, with the identity of its BeaconRegistered event.
  beacon: RegisteredRoundBeacon;
  requestId: bigint; consumer: string; clientSeed: string; mapping: MappingSpec; mappingHash: string;
  requestBlock: bigint; requestTimestamp: bigint; deadline: bigint;
  beaconId: number; round: bigint;
  /// The round's signature (RoundVerified) and the randomness the coordinator cached for it.
  roundSignature: string; roundRandomness: string;
  /// The proof as FulfillmentEvidence carries it, and what the coordinator stored: randomness, proofHash and transcriptHash.
  proof: VRFProof; randomness: string; proofHash: string; transcriptHash: string;
  /// The FulfillmentEvidence packet and getMappedResult's values, when the replay should check them too.
  evidencePacket?: string; mappedResult?: readonly bigint[];
  /// The timestamp of the block that accepted the fulfilment.
  acceptanceTimestamp: bigint;
}
export type RoundCheckName = "keyHash" | "configuration" | "beaconIdentity" | "beaconInForce" | "assignment" | "roundSignature" | "roundRandomness"
  | "seed" | "proof" | "randomness" | "proofHash" | "evidencePacket" | "transcript" | "mappingHash" | "mappedResult" | "deadline" | "acceptance";
export interface RoundCheck { name: RoundCheckName; ok: boolean; detail?: string; }
/// Every check, each recomputing one recorded value from the recorded values it depends on, so that a wrong value fails the checks that
/// read it and no other. valid is true when every check passes.
export interface RoundReplayVerdict {
  valid: boolean; checks: RoundCheck[]; failed: RoundCheckName[];
  /// Recomputed from the evidence, when its inputs are well formed: the seed, the round's scheduled time, how long after the request it
  /// was scheduled, and the mapped values of the recorded randomness.
  seed?: bigint; roundTime?: bigint; roundLeadSeconds?: bigint; mappedResult?: bigint[];
}

const PROOF = "tuple(uint256[2] pk,uint256[2] gamma,uint256 c,uint256 s,uint256 seed,address uWitness,uint256[2] cGammaWitness,uint256[2] sHashWitness,uint256 zInv)";
const EVIDENCE_PACKET_BYTES = 416;
/// The VRF output a proof's gamma gives, valid or not: keccak256(abi.encode(3, gamma)), as VRF.sol computes it.
const outputOf = (proof: VRFProof) => keccak256(abi.encode(["uint256", "uint256[2]"], [3n, proof.gamma]));

/// Replay one fulfilled request from its evidence: the configuration and key hash, the beacon's identity (and, given the schedule, that
/// the beacon was in force), the round assignment, the round's signature and randomness, the seed, the VRF proof and its output, the
/// proof hash and evidence packet, the transcript, the mapping and its result, the deadline and the acceptance time. It never throws:
/// malformed evidence fails the checks that read it.
export function replayRoundRequest(evidence: RoundRequestEvidence, options: { schedule?: RoundSchedule } = {}): RoundReplayVerdict {
  const e = evidence, checks: RoundCheck[] = [];
  const check = (name: RoundCheckName, run: () => boolean | string) => {
    try {
      const answer = run();
      checks.push(answer === true ? { name, ok: true } : { name, ok: false, detail: answer === false ? undefined : answer });
    } catch (error) {
      checks.push({ name, ok: false, detail: error instanceof Error ? error.message : String(error) });
    }
  };
  const attempt = <T>(run: () => T): T | undefined => { try { return run(); } catch { return undefined; } };
  const seed = attempt(() => roundSeed({ chainId: e.chainId, coordinator: e.coordinator, keyHash: e.keyHash, requestId: e.requestId,
    consumer: e.consumer, clientSeed: e.clientSeed, mappingHash: e.mappingHash, requestBlock: e.requestBlock, beaconId: e.beaconId,
    round: e.round, roundRandomness: e.roundRandomness }));
  const scheduled = attempt(() => roundTime(e.beacon, e.round));
  const mapped = attempt(() => mapRandomness(e.randomness, e.mapping));

  check("keyHash", () => sameHex(hashPublicKey(e.configuration.publicKey), e.keyHash));
  check("configuration", () => sameHex(roundConfigurationHash(e.configuration), e.configurationHash));
  check("beaconIdentity", () => {
    if (!sameHex(roundBeaconIdentity(e.beacon), e.beacon.identity)) return "the registration does not hash to its identity";
    // The configuration binds beacon 0's identity.
    return e.beaconId !== 0 || sameHex(e.configuration.beaconIdentity, e.beacon.identity) || "beacon 0 is not the configuration's beacon";
  });
  if (options.schedule) check("beaconInForce", () => {
    const { beaconId, beacon } = roundAt(options.schedule!, e.requestTimestamp);
    return beaconId === e.beaconId && sameHex(beacon.identity, e.beacon.identity) || `beacon ${beaconId} was in force at the request`;
  });
  check("assignment", () => roundFor(e.beacon, e.requestTimestamp) === e.round);
  check("roundSignature", () => verifyRoundSignature(e.beacon.publicKey, e.round, e.roundSignature));
  check("roundRandomness", () => sameHex(roundRandomness(e.roundSignature), e.roundRandomness));
  check("seed", () => seed !== undefined && seed === e.proof.seed);
  check("proof", () => {
    const verified = verifyVRFProof(e.proof, e.configuration.publicKey, e.proof.seed);
    return verified.valid || verified.reason;
  });
  check("randomness", () => sameHex(outputOf(e.proof), e.randomness));
  check("proofHash", () => sameHex(hashProof(e.proof), e.proofHash));
  if (e.evidencePacket !== undefined)
    check("evidencePacket", () => getBytes(e.evidencePacket!).length === EVIDENCE_PACKET_BYTES && sameHex(e.evidencePacket!, abi.encode([PROOF], [e.proof])));
  check("transcript", () => sameHex(roundTranscriptHash({ chainId: e.chainId, coordinator: e.coordinator, requestId: e.requestId,
    configurationHash: e.configurationHash, beaconId: e.beaconId, round: e.round, roundRandomness: e.roundRandomness, proofHash: e.proofHash,
    randomness: e.randomness, mappingHash: e.mappingHash }), e.transcriptHash));
  check("mappingHash", () => sameHex(hashMapping(e.mapping), e.mappingHash));
  if (e.mappedResult !== undefined)
    check("mappedResult", () => mapped !== undefined && mapped.length === e.mappedResult!.length && mapped.every((value, i) => value === e.mappedResult![i]));
  check("deadline", () => e.deadline === roundDeadline(e.requestTimestamp));
  check("acceptance", () => e.acceptanceTimestamp >= e.requestTimestamp && e.acceptanceTimestamp <= e.deadline);

  const failed = checks.filter(c => !c.ok).map(c => c.name);
  return { valid: failed.length === 0, checks, failed, seed, roundTime: scheduled,
    roundLeadSeconds: scheduled === undefined ? undefined : scheduled - e.requestTimestamp, mappedResult: mapped };
}
