// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

/// @notice Version 1 deterministic, unbiased range sampling and bounded game operations.
/// @dev Choice/shuffle outputs are indices. Consumers must lock their item list before requesting.
library RandomnessMapping {
    enum Operation { Raw, DiceRoll, CoinFlip, NumberRange, ChooseOne, ChooseMany, Shuffle }
    struct Spec {
        Operation operation;
        uint256 lower;
        uint256 upper;
        uint32 count;
        uint32 population;
    }
    struct Stream { bytes32 randomness; uint256 cursor; }
    bytes32 internal constant MAP_DOMAIN = keccak256("D20_MAP");
    uint32 internal constant MAX_ITEMS = 256;
    uint32 internal constant MAX_DICE = 128;
    error InvalidMapping();

    function validate(Spec memory s) internal pure {
        if (s.operation == Operation.Raw) {
            if (s.lower != 0 || s.upper != 0 || s.count != 0 || s.population != 0) revert InvalidMapping();
        } else if (s.operation == Operation.DiceRoll) {
            if (s.lower != 0 || s.upper < 2 || s.count == 0 || s.count > MAX_DICE || s.population != 0)
                revert InvalidMapping();
        } else if (s.operation == Operation.CoinFlip) {
            if (s.lower != 0 || s.upper != 0 || s.count != 1 || s.population != 0) revert InvalidMapping();
        } else if (s.operation == Operation.NumberRange) {
            if (s.lower > s.upper || s.count != 1 || s.population != 0) revert InvalidMapping();
        } else {
            if (s.lower != 0 || s.upper != 0 || s.population == 0 || s.population > MAX_ITEMS)
                revert InvalidMapping();
            if (s.operation == Operation.ChooseOne && s.count != 1) revert InvalidMapping();
            if (s.operation == Operation.ChooseMany && (s.count == 0 || s.count > s.population))
                revert InvalidMapping();
            if (s.operation == Operation.Shuffle && s.count != s.population) revert InvalidMapping();
        }
    }

    function hash(Spec memory s) internal pure returns (bytes32) {
        return keccak256(abi.encode(s.operation, s.lower, s.upper, s.count, s.population));
    }

    function map(bytes32 randomness, Spec memory s) internal pure returns (uint256[] memory result) {
        validate(s);
        Stream memory stream = Stream(randomness, 0);
        if (s.operation == Operation.Raw) {
            result = new uint256[](1);
            result[0] = uint256(randomness);
        } else if (s.operation == Operation.DiceRoll) {
            result = new uint256[](s.count);
            for (uint256 i; i < s.count; ++i) result[i] = sample(stream, s.upper) + 1;
        } else if (s.operation == Operation.CoinFlip) {
            result = new uint256[](1);
            result[0] = sample(stream, 2); // 0 = tails, 1 = heads.
        } else if (s.operation == Operation.NumberRange) {
            result = new uint256[](1);
            result[0] = s.lower == 0 && s.upper == type(uint256).max
                ? next(stream) : s.lower + sample(stream, s.upper - s.lower + 1);
        } else if (s.operation == Operation.ChooseOne) {
            result = new uint256[](1);
            result[0] = sample(stream, s.population);
        } else {
            // Partial Fisher-Yates: without-replacement choices, or a full permutation.
            uint256[] memory pool = new uint256[](s.population);
            for (uint256 i; i < s.population; ++i) pool[i] = i;
            result = new uint256[](s.count);
            for (uint256 i; i < s.count; ++i) {
                uint256 j = i + sample(stream, s.population - i);
                (pool[i], pool[j]) = (pool[j], pool[i]);
                result[i] = pool[i];
            }
        }
    }

    function next(Stream memory stream) internal pure returns (uint256) {
        return uint256(keccak256(abi.encode(MAP_DOMAIN, stream.randomness, stream.cursor++)));
    }

    /// @dev Reject the short residue prefix instead of introducing modulo bias.
    function sample(Stream memory stream, uint256 bound) internal pure returns (uint256) {
        uint256 threshold;
        unchecked { threshold = (0 - bound) % bound; }
        uint256 word;
        do { word = next(stream); } while (word < threshold);
        return word % bound;
    }
}
