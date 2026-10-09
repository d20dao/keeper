// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {BLS} from "./vendor/bls-bn254/BLS.sol";
import {IBeaconVerifier} from "./interfaces/IBeaconVerifier.sol";

/// @notice Stateless verifier of drand beacon rounds under the bls-bn254-unchained-on-g1 scheme of drand's evmnet: the
/// group public key is a BN254 G2 point, and round r's signature is a G1 point on the RFC 9380 hash-to-curve of
/// keccak256(r as 8 big-endian bytes). Keys and signatures use drand's serialization of 32-byte big-endian words, x ‖ y
/// for G1 and x_im ‖ x_re ‖ y_im ‖ y_re for G2, which is also the word order of the EIP-197 pairing input.
/// @dev Built on the unmodified kevincharm/bls-bn254 library in contracts/vendor/bls-bn254. Invalid input returns false
/// without consuming the caller's gas: points are checked before the pairing, and the pairing precompile, which burns
/// all gas it is given when a point is invalid, gets a fixed allowance. A valid signature never reads as invalid for want of
/// gas: when too little is left for the pairing allowance, verifyRound reverts InsufficientGas instead of answering.
contract D20BeaconVerifier is IBeaconVerifier {
    /// @notice Too little gas was left for the pairing precompile to get its whole allowance.
    error InsufficientGas();
    /// @notice RFC 9380 domain separation tag of the scheme's hash-to-curve.
    bytes public constant DST = "BLS_SIG_BN254G1_XMD:KECCAK-256_SVDW_RO_NUL_";
    /// @notice Gas given to the pairing precompile. A two-pair check costs 113,000 under EIP-1108.
    uint256 public constant PAIRING_GAS = 200_000;
    // The negated G2 generator in pairing word order (x_im, x_re, y_im, y_re).
    uint256 private constant NEG_G2_X_IM = 11559732032986387107991004021392285783925812861821192530917403151452391805634;
    uint256 private constant NEG_G2_X_RE = 10857046999023057135944570762232829481370756359578518086990519993285655852781;
    uint256 private constant NEG_G2_Y_IM = 17805874995975841540914202342111839520379459829704422454583296818431106115052;
    uint256 private constant NEG_G2_Y_RE = 13392588948715843804641432497768002650278120570034223513918757245338268106653;

    /// @notice Whether publicKey is 128 bytes encoding a point of the G2 curve with coordinates below the field order.
    /// Subgroup membership is not checked here: the pairing precompile rejects a key outside the subgroup, so a verified
    /// signature under the key establishes it.
    function isValidPublicKey(bytes calldata publicKey) public pure returns (bool) {
        if (publicKey.length != 128) return false;
        return BLS.isValidPublicKey(_curveWords(publicKey));
    }

    /// @notice The G1 point that the signature of round signs.
    function roundMessage(uint64 round) public view returns (uint256[2] memory) {
        return BLS.hashToPoint(DST, abi.encodePacked(keccak256(abi.encodePacked(round))));
    }

    /// @notice Whether signature (64 bytes) is the beacon's signature of round under publicKey (128 bytes).
    /// @dev Input that fails a check before the pairing returns false. Otherwise the pairing needs its gas: with less it reverts
    /// InsufficientGas rather than answer false. A direct call needs a gas limit of about 285,000.
    function verifyRound(bytes calldata publicKey, uint64 round, bytes calldata signature) external view returns (bool) {
        if (signature.length != 64 || !isValidPublicKey(publicKey)) return false;
        uint256[2] memory sigma = [uint256(bytes32(signature[0:32])), uint256(bytes32(signature[32:64]))];
        // Coordinates below the field order and on the curve. The zero encoding of the point at infinity is not on the
        // curve, and G1 has cofactor 1, so this is the whole group check.
        if (!BLS.isValidSignature(sigma)) return false;
        // e(sigma, -G2) * e(H(round), publicKey) == 1, that is e(sigma, G2) == e(H(round), publicKey).
        bytes memory input = bytes.concat(abi.encodePacked(sigma, [NEG_G2_X_IM, NEG_G2_X_RE, NEG_G2_Y_IM, NEG_G2_Y_RE]),
            abi.encodePacked(roundMessage(round)), publicKey);
        // The call forwards at most 63/64 of what is left after its own cost (EIP-150), so the allowance needs PAIRING_GAS + PAIRING_GAS / 63
        // left, and 1,000 more cover the rounding, the 100 a precompile call costs and the opcodes before it. With less, a valid signature
        // could be cut short inside the precompile and read as invalid, so this reverts instead. Invalid input was refused above,
        // and it returned false without needing this gas.
        if (gasleft() < PAIRING_GAS + PAIRING_GAS / 63 + 1_000) revert InsufficientGas();
        (bool ok, bytes memory out) = address(8).staticcall{gas: PAIRING_GAS}(input);
        return ok && out.length == 32 && abi.decode(out, (uint256)) == 1;
    }

    /// @dev BLS.sol's curve check takes G2 coordinates as (x_re, x_im, y_re, y_im).
    function _curveWords(bytes calldata key) private pure returns (uint256[4] memory point) {
        point[0] = uint256(bytes32(key[32:64]));
        point[1] = uint256(bytes32(key[0:32]));
        point[2] = uint256(bytes32(key[96:128]));
        point[3] = uint256(bytes32(key[64:96]));
    }
}
