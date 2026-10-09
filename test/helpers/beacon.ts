import { concat, id, toBeHex } from "ethers";
import { bn254 } from "@noble/curves/bn254";
import { beaconRoundAt, beaconRoundTime, encodeBeaconRound, type BeaconRegistration } from "../../src/index.ts";
import { beaconRoundMessage } from "../../src/beacon.ts";
const G1 = bn254.G1.Point, G2 = bn254.G2.Point;
const wordHex = (value: bigint) => toBeHex(value, 32);
/// Group public key of a secret scalar in drand's encoding. For tests and local fake beacons.
export function beaconPublicKey(secretKey: bigint): string {
  const { x, y } = G2.BASE.multiply(secretKey).toAffine();
  return concat([x.c1, x.c0, y.c1, y.c0].map(wordHex));
}
/// A secret scalar's signature of a round, sk × H(round) as x ‖ y. For tests and local fake beacons.
export function signBeaconRound(secretKey: bigint, round: bigint): string {
  const { x, y } = G1.fromAffine(beaconRoundMessage(round)).multiply(secretKey).toAffine();
  return concat([x, y].map(wordHex));
}
// A public test beacon whose secret key the tests know, so they can sign any round. Never a production key.
const SECRET = BigInt(id("D20 test beacon secret")) % bn254.fields.Fr.ORDER;
/// The test beacon as registered with a verifier: rounds every 3 seconds from a genesis 3000 seconds before now, so the round
/// current at any block the tests mine is fresh.
export function testBeacon(verifier: string, now: bigint): BeaconRegistration {
  return { verifier, chainHash: id("D20 test beacon network"), publicKey: beaconPublicKey(SECRET), genesis: now - 3000n, period: 3n };
}
/// The test beacon's signature of a round.
export const signTestRound = (round: bigint) => signBeaconRound(SECRET, round);
/// A well-formed group key that the test beacon's signatures do not verify under.
export const otherTestKey = () => beaconPublicKey(SECRET + 1n);
/// The beacon's round current at a time, as the registry commits it: its number as data, its scheduled time and its signature.
export function beaconAttestation(beacon: Pick<BeaconRegistration, "genesis" | "period">, time: bigint) {
  const round = beaconRoundAt(beacon, time);
  return { timestamp: beaconRoundTime(beacon, round), data: encodeBeaconRound(round), signature: signTestRound(round) };
}
/// registerBeacon for a test beacon, vouched for by its signature of a past round.
export const registerTestBeacon = (registry: any, beacon: BeaconRegistration, sampleRound = 1n) =>
  registry.registerBeacon(beacon.verifier, beacon.chainHash, beacon.publicKey, beacon.genesis, beacon.period, sampleRound, signTestRound(sampleRound));
