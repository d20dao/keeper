# Vendored VRF verifier

- Upstream: https://github.com/smartcontractkit/chainlink-evm
- Commit: `b6cf3a6ff90c7172e1d6090d056c0f77282450ce`
- File: `contracts/src/v0.8/vrf/VRF.sol`
- Raw source: https://raw.githubusercontent.com/smartcontractkit/chainlink-evm/b6cf3a6ff90c7172e1d6090d056c0f77282450ce/contracts/src/v0.8/vrf/VRF.sol
- SHA-256: `98e0345fd2dd42178cad4ad16dd8b18621112078d63d0c64839c8bd01c539350`
- License: MIT as declared in the source. The upstream root license is preserved in `CHAINLINK-LICENSE` (including notices for upstream components not vendored here).
- Modifications: none. `npm run vendor:check` verifies the original bytes.

This is the Chainlink secp256k1/Keccak VRF construction described in that source, with its documented differences from the referenced IETF draft. It is not advertised as RFC 9381 wire-format interoperability. Reusing this file neither makes D20DAO a Chainlink service nor extends any upstream audit to our coordinator, mapping, consumer or keeper.
