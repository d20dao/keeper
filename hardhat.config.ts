import { readdirSync } from "node:fs";
import { defineConfig } from "hardhat/config";
import toolbox from "@nomicfoundation/hardhat-toolbox-mocha-ethers";

const settings = { optimizer: { enabled: true, runs: 200 }, evmVersion: "cancun",
  outputSelection: { "*": { "*": ["abi", "evm.bytecode", "evm.deployedBytecode", "evm.methodIdentifiers", "metadata", "storageLayout"], "": ["ast"] } } };
// The Robinhood contracts compile with the IR pipeline, which with their two helpers keeps the round coordinator under EIP-170's 24,576
// bytes. The override names each source file under contracts/robinhood/; every other contract keeps the settings above.
const robinhood = readdirSync(new URL("./contracts/robinhood/", import.meta.url), { recursive: true, encoding: "utf8" })
  .filter((file) => file.endsWith(".sol")).map((file) => `contracts/robinhood/${file.replaceAll("\\", "/")}`).sort();

export default defineConfig({
  plugins: [toolbox],
  solidity: {
    compilers: [{ version: "0.8.28", settings }],
    overrides: Object.fromEntries(robinhood.map((file) => [file, { version: "0.8.28", settings: { ...settings, viaIR: true } }])),
  },
  networks: {
    local: { type: "edr-simulated", chainId: 31337 },
    // Local approximation only: no Arc consensus, RPC provider limits or EWMA implementation.
    loadSim: { type: "edr-simulated", chainId: 31337, hardfork: "cancun", allowBlocksWithSameTimestamp: true,
      blockGasLimit: 30000000, initialBaseFeePerGas: 20000000000 },
  },
});
