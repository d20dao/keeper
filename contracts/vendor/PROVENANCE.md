# Vendored VRF verifier

- Upstream: https://github.com/smartcontractkit/chainlink-evm
- Commit: `b6cf3a6ff90c7172e1d6090d056c0f77282450ce`
- File: `contracts/src/v0.8/vrf/VRF.sol`
- Raw source: https://raw.githubusercontent.com/smartcontractkit/chainlink-evm/b6cf3a6ff90c7172e1d6090d056c0f77282450ce/contracts/src/v0.8/vrf/VRF.sol
- SHA-256: `98e0345fd2dd42178cad4ad16dd8b18621112078d63d0c64839c8bd01c539350`
- License: MIT as declared in the source. The upstream root license is preserved in `CHAINLINK-LICENSE` (including notices for upstream components not vendored here).
- Modifications: none. `npm run vendor:check` verifies the original bytes.

This is the Chainlink secp256k1/Keccak VRF construction described in that source, with its documented differences from the referenced IETF draft. It is not advertised as RFC 9381 wire-format interoperability. Reusing this file neither makes D20DAO a Chainlink service nor extends any upstream audit to our coordinator, mapping, consumer or keeper.

# Vendored BLS library on BN254

- Upstream: https://github.com/kevincharm/bls-bn254
- Commit: `9f70fb4dff2cd0921dc2144929dc0aff6f21a9b9` (release 2.0.0)
- Files, in `bls-bn254/`:
  - `contracts/BLS.sol`, SHA-256 `887fd94553b9d6d0a247900bdb05523d3b9ef3b8ca049e2a0524a0b1a9d37e69`
  - `contracts/ModExp.sol`, SHA-256 `fd91ae9291511668914b05fece03fb9bf6e2b57f7784d2edddc82456af12b495`
- License: MIT, preserved in `bls-bn254/LICENSE`, SHA-256 `af36460fa628a7aca8c5b1f1b6b3615376f00212284fa0fd61a013ee982a3666`.
- Modifications: none. `npm run vendor:check` verifies the original bytes, the license included.

`D20BeaconVerifier` uses this library's RFC 9380 hash-to-curve onto G1 (expand_message_xmd with keccak256 and the Shallue-van de Woestijne map) and its point checks. These are the operations of drand's `bls-bn254-unchained-on-g1` scheme. The verifier runs the pairing itself, with a fixed gas allowance, instead of the library's `verifySingle`, which forwards all remaining gas to the precompile. Reusing this file neither makes D20DAO a drand or League of Entropy service nor extends any upstream review to our registry or keeper.
