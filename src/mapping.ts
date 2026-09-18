import { AbiCoder, id, keccak256 } from "ethers";

export const Operation = { Raw: 0, DiceRoll: 1, CoinFlip: 2, NumberRange: 3, ChooseOne: 4, ChooseMany: 5, Shuffle: 6 } as const;
export interface MappingSpec { operation: number; lower: bigint; upper: bigint; count: number; population: number; }
const MAX = (1n << 256n) - 1n;
const abi = AbiCoder.defaultAbiCoder();
const MAP_DOMAIN = id("D20_MAP");
function spec(operation: number, lower = 0n, upper = 0n, count = 1, population = 0): MappingSpec {
  const result = { operation, lower, upper, count, population };
  validateMapping(result);
  return result;
}
export const builtins = {
  raw: (): MappingSpec => spec(Operation.Raw, 0n, 0n, 0),
  diceRoll: (sides: bigint, count = 1): MappingSpec => spec(Operation.DiceRoll, 0n, sides, count),
  dN: (sides: bigint): MappingSpec => spec(Operation.DiceRoll, 0n, sides),
  d20: (): MappingSpec => spec(Operation.DiceRoll, 0n, 20n),
  d12: (): MappingSpec => spec(Operation.DiceRoll, 0n, 12n),
  d10: (): MappingSpec => spec(Operation.DiceRoll, 0n, 10n),
  d8: (): MappingSpec => spec(Operation.DiceRoll, 0n, 8n),
  d6: (): MappingSpec => spec(Operation.DiceRoll, 0n, 6n),
  d4: (): MappingSpec => spec(Operation.DiceRoll, 0n, 4n),
  coinFlip: (): MappingSpec => spec(Operation.CoinFlip),
  numberRange: (min: bigint, max: bigint): MappingSpec => spec(Operation.NumberRange, min, max),
  chooseOne: (size: number): MappingSpec => spec(Operation.ChooseOne, 0n, 0n, 1, size),
  chooseMany: (size: number, count: number): MappingSpec => spec(Operation.ChooseMany, 0n, 0n, count, size),
  shuffle: (size: number): MappingSpec => spec(Operation.Shuffle, 0n, 0n, size, size),
};

export function validateMapping(s: MappingSpec): void {
  const bad = () => { throw new Error("Invalid mapping parameters"); };
  if (!Number.isInteger(s.operation) || s.operation < 0 || s.operation > 6 ||
      !Number.isInteger(s.count) || !Number.isInteger(s.population) ||
      s.count < 0 || s.population < 0 || s.lower < 0n || s.upper < 0n || s.lower > MAX || s.upper > MAX) bad();
  switch (s.operation) {
    case Operation.Raw:
      if (s.lower !== 0n || s.upper !== 0n || s.count !== 0 || s.population !== 0) bad(); break;
    case Operation.DiceRoll:
      if (s.lower !== 0n || s.upper < 2n || s.count < 1 || s.count > 128 || s.population !== 0) bad(); break;
    case Operation.CoinFlip:
      if (s.lower !== 0n || s.upper !== 0n || s.count !== 1 || s.population !== 0) bad(); break;
    case Operation.NumberRange:
      if (s.lower > s.upper || s.count !== 1 || s.population !== 0) bad(); break;
    default:
      if (s.lower !== 0n || s.upper !== 0n || s.population < 1 || s.population > 256) bad();
      if (s.operation === Operation.ChooseOne && s.count !== 1) bad();
      if (s.operation === Operation.ChooseMany && (s.count < 1 || s.count > s.population)) bad();
      if (s.operation === Operation.Shuffle && s.count !== s.population) bad();
  }
}

export function hashMapping(s: MappingSpec): string {
  validateMapping(s);
  return keccak256(abi.encode(["uint8", "uint256", "uint256", "uint32", "uint32"],
    [s.operation, s.lower, s.upper, s.count, s.population]));
}

export function mapRandomness(randomness: string, s: MappingSpec): bigint[] {
  validateMapping(s);
  if (!/^0x[0-9a-fA-F]{64}$/.test(randomness)) throw new Error("Expected bytes32 randomness");
  let cursor = 0n;
  const next = () => BigInt(keccak256(abi.encode(["bytes32", "bytes32", "uint256"], [MAP_DOMAIN, randomness, cursor++])));
  const sample = (bound: bigint) => {
    const threshold = (1n << 256n) % bound;
    let word: bigint;
    do { word = next(); } while (word < threshold);
    return word % bound;
  };
  switch (s.operation) {
    case Operation.Raw: return [BigInt(randomness)];
    case Operation.DiceRoll: return Array.from({ length: s.count }, () => sample(s.upper) + 1n);
    case Operation.CoinFlip: return [sample(2n)];
    case Operation.NumberRange:
      return [s.lower === 0n && s.upper === MAX ? next() : s.lower + sample(s.upper - s.lower + 1n)];
    case Operation.ChooseOne: return [sample(BigInt(s.population))];
    default: {
      const pool = Array.from({ length: s.population }, (_, i) => BigInt(i));
      const output: bigint[] = [];
      for (let i = 0; i < s.count; i++) {
        const j = i + Number(sample(BigInt(s.population - i)));
        [pool[i], pool[j]] = [pool[j], pool[i]];
        output.push(pool[i]);
      }
      return output;
    }
  }
}
