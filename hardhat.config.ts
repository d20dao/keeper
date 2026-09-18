import { defineConfig } from "hardhat/config";
import toolbox from "@nomicfoundation/hardhat-toolbox-mocha-ethers";

export default defineConfig({
  plugins: [toolbox],
  solidity: {
    version: "0.8.28",
    settings: { optimizer: { enabled: true, runs: 200 }, evmVersion: "cancun",
      outputSelection: { "*": { "*": ["abi", "evm.bytecode", "evm.deployedBytecode", "evm.methodIdentifiers", "metadata", "storageLayout"], "": ["ast"] } } },
  },
  networks: {
    local: { type: "edr-simulated", chainId: 31337 },
    // Local approximation only: no Arc consensus, RPC provider limits or EWMA implementation.
    loadSim: { type: "edr-simulated", chainId: 31337, hardfork: "cancun", allowBlocksWithSameTimestamp: true,
      blockGasLimit: 30000000, initialBaseFeePerGas: 20000000000 },
  },
});
