// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

/// @notice Checks a public randomness beacon's signature of one round, for example D20BeaconVerifier for drand's
/// bls-bn254-unchained-on-g1 scheme. EpochEntropy records a verifier in each beacon registration.
interface IBeaconVerifier {
    /// @notice Whether publicKey is well formed: the right length and encoding of a point on the scheme's curve. That is all it
    /// checks. Membership of the prime-order subgroup is not established here but by verifying a signature under the key.
    function isValidPublicKey(bytes calldata publicKey) external view returns (bool);
    /// @notice Whether signature is the beacon's signature of round under publicKey. Returns false for invalid input.
    function verifyRound(bytes calldata publicKey, uint64 round, bytes calldata signature) external view returns (bool);
}
