// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {BeaconBook} from "../BeaconBook.sol";

/// @dev TEST ONLY. Declares BeaconBook's ERC-7201 namespace struct as its only state variable, so that the compiler's storage layout output
/// carries the struct's members, slots and offsets for the storage baseline (scripts/check-storage-layout.mjs). Never deployed.
contract BeaconBookStorageProbe {
    BeaconBook.BeaconBookStorage private book;
}
