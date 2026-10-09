//! The round coordinator's ABI (`contracts/robinhood/D20VRFCoordinatorRobinhood.sol` and its base `BeaconBook.sol`), with
//! its own names: a request bound to a drand round (`getRoundRequest`), the keeper roles of the coordinator, the beacon
//! book and the round fulfillments. A keeper in round mode (COORDINATOR_KIND=round) reads and writes the round
//! coordinator through these bindings alone, never through `abi::Coordinator` or `abi::EpochRegistry`.
//!
//! Some functions have the selectors of functions of the epoch coordinator: those of `ID20VRF` and of the coordinator's
//! request scan (`nextRequestId`, `getPendingRequestIds`, `requestFeePaid`, `feeRecipient`, `publicKeyX`/`Y`,
//! `protocolConfigurationHash`). A keeper tells them apart by the contract it asks. None has a selector of an epoch
//! registry's function: the beacon book's reads are `getBeacon` and `checkRoundSignature`, and its constants
//! `ROUND_BEACON_DOMAIN` and `ROUND_VERIFY_GAS`.
// The binding of RandomnessRequested has a constructor with one argument per field of the event.
#![allow(clippy::too_many_arguments)]
use alloy_sol_types::sol;
sol! {
    /// The VRF proof the round coordinator verifies: the tuple of the epoch coordinator's (`VRF.Proof`).
    #[derive(Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    struct RoundProof {
        uint256[2] pk; uint256[2] gamma; uint256 c; uint256 s; uint256 seed; address uWitness;
        uint256[2] cGammaWitness; uint256[2] sHashWitness; uint256 zInv;
    }
    /// Everything about a request in one read: its seed inputs, the round it is bound to, its result and its status.
    /// `roundRandomness` is zero while the round is not verified on chain.
    #[derive(Debug, Default, PartialEq, Eq)]
    struct RoundRequest {
        address consumer; uint32 callbackGasLimit; uint64 requestBlock; uint64 deadline; address refundAddress;
        bytes32 clientSeed; bytes32 mappingHash; uint8 beaconId; uint64 round; bytes32 roundRandomness;
        bytes32 randomness; bytes32 proofHash; bytes32 transcriptHash; uint256 feePaid;
        bool fulfilled; bool delivered; bool refunded;
    }
    /// The signature of one round a batch lists, for the members whose round is not verified on chain yet.
    #[derive(Debug, PartialEq, Eq)]
    struct RoundSignature { uint8 beaconId; uint64 round; bytes signature; }
    /// A registered beacon: its verifier, its schedule and its key.
    #[derive(Debug, PartialEq, Eq)]
    struct Beacon { address verifier; uint64 genesis; uint64 period; bytes32 chainHash; bytes publicKey; }
    interface RoundCoordinator {
        // The request scan and the request.
        function nextRequestId() external view returns (uint256 next);
        function getPendingRequestIds(uint256 fromId, uint256 limit) external view returns (uint256[] ids, uint256 nextCursor);
        function getRoundRequest(uint256 requestId) external view returns (RoundRequest request);
        function requestFeePaid(uint256 requestId) external view returns (uint256 fee);
        // The VRF key and the configuration.
        function publicKeyX() external view returns (uint256 x);
        function publicKeyY() external view returns (uint256 y);
        function keyHash() external view returns (bytes32 hash);
        function protocolConfigurationHash() external view returns (bytes32 hash);
        function pricing() external view returns (uint256 minFee, uint16 feeMultiplier, uint32 fulfillGasOverhead);
        function keeperFeeBps() external view returns (uint16 bps);
        function feeRecipient() external view returns (address recipient);
        // The keeper roles.
        function keeper() external view returns (address primary);
        function isBackupKeeper(address account) external view returns (bool allowed);
        function isAuthorizedKeeper(address account) external view returns (bool allowed);
        function backupKeeperCount() external view returns (uint256 count);
        // The beacon book.
        function ROUND_LEAD() external view returns (uint64 lead);
        function beaconCount() external view returns (uint256 count);
        function getBeacon(uint8 beaconId) external view returns (Beacon beacon);
        function beaconSchedule() external view returns (uint8 beaconId, uint64 since, uint8 nextBeaconId, uint64 nextFrom);
        function roundAt(uint64 timestamp) external view returns (uint8 beaconId, uint64 round);
        function roundTime(uint8 beaconId, uint64 round) external view returns (uint256 time);
        function roundRandomness(uint8 beaconId, uint64 round) external view returns (bytes32 randomness);
        /// Whether `signature` is the beacon's signature of `round`, asked of its verifier under ROUND_VERIFY_GAS. An
        /// `eth_call` of it needs about 460,000 gas.
        function checkRoundSignature(uint8 beaconId, uint64 round, bytes signature) external view returns (bool valid);
        function ROUND_BEACON_DOMAIN() external view returns (bytes32 domain);
        function ROUND_VERIFY_GAS() external view returns (uint256 gas);
        // The proof input and the fulfillments.
        function requestSeed(uint256 requestId) external view returns (uint256 seed);
        function getProofContext(uint256 requestId, bytes roundSignature) external view returns (uint256 seed, uint64 deadline, bool fulfilled, bool refunded);
        function verifyRequestProof(uint256 requestId, RoundProof proof) external view returns (bytes32 randomness);
        function fulfillRandomness(uint256 requestId, RoundProof proof, bytes roundSignature) external;
        function fulfillRandomnessBatch(RoundSignature[] rounds, uint256[] ids, RoundProof[] proofs) external;
        // The events the keeper reads.
        event RandomnessRequested(
            uint256 indexed requestId, address indexed consumer, bytes32 indexed keyHash,
            bytes32 clientSeed, uint64 requestBlock, uint32 callbackGasLimit, uint256 feePaid,
            address refundAddress, uint64 deadline
        );
        event RoundAssigned(uint256 indexed requestId, uint8 indexed beaconId, uint64 indexed round);
        event RoundVerified(uint8 indexed beaconId, uint64 indexed round, bytes32 randomness, bytes signature);
        event RandomnessFulfilled(uint256 indexed requestId, bytes32 randomness, address indexed submitter);
        event RequestServed(uint256 indexed requestId, uint256 indexed serveIndex);
        /// Reason: 1 already fulfilled, 2 refunded, 3 past deadline. The member changes no state.
        event FulfillmentSkipped(uint256 indexed requestId, uint8 reason);
        event KeeperChanged(address indexed previousKeeper, address indexed newKeeper);
        event BackupKeeperSet(address indexed account, bool allowed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::keccak256;
    use alloy_sol_types::{SolCall, SolEvent};

    fn selector(signature: &str) -> [u8; 4] {
        keccak256(signature).0[..4].try_into().unwrap()
    }

    /// The selectors and topics are the contract's: computed here from the Solidity signatures of
    /// `D20VRFCoordinatorRobinhood.sol` and `BeaconBook.sol`, not from the binding.
    #[test]
    fn the_binding_has_the_round_coordinators_selectors_and_topics() {
        use RoundCoordinator as R;
        let proof =
            "(uint256[2],uint256[2],uint256,uint256,uint256,address,uint256[2],uint256[2],uint256)";
        for (binding, signature) in [
            (
                R::getRoundRequestCall::SELECTOR,
                "getRoundRequest(uint256)".to_owned(),
            ),
            (R::keeperCall::SELECTOR, "keeper()".to_owned()),
            (
                R::isBackupKeeperCall::SELECTOR,
                "isBackupKeeper(address)".to_owned(),
            ),
            (
                R::isAuthorizedKeeperCall::SELECTOR,
                "isAuthorizedKeeper(address)".to_owned(),
            ),
            (
                R::backupKeeperCountCall::SELECTOR,
                "backupKeeperCount()".to_owned(),
            ),
            (R::keyHashCall::SELECTOR, "keyHash()".to_owned()),
            (R::ROUND_LEADCall::SELECTOR, "ROUND_LEAD()".to_owned()),
            (R::pricingCall::SELECTOR, "pricing()".to_owned()),
            (R::keeperFeeBpsCall::SELECTOR, "keeperFeeBps()".to_owned()),
            (R::beaconCountCall::SELECTOR, "beaconCount()".to_owned()),
            (R::getBeaconCall::SELECTOR, "getBeacon(uint8)".to_owned()),
            (
                R::checkRoundSignatureCall::SELECTOR,
                "checkRoundSignature(uint8,uint64,bytes)".to_owned(),
            ),
            (
                R::ROUND_BEACON_DOMAINCall::SELECTOR,
                "ROUND_BEACON_DOMAIN()".to_owned(),
            ),
            (
                R::ROUND_VERIFY_GASCall::SELECTOR,
                "ROUND_VERIFY_GAS()".to_owned(),
            ),
            (
                R::beaconScheduleCall::SELECTOR,
                "beaconSchedule()".to_owned(),
            ),
            (R::roundAtCall::SELECTOR, "roundAt(uint64)".to_owned()),
            (
                R::roundTimeCall::SELECTOR,
                "roundTime(uint8,uint64)".to_owned(),
            ),
            (
                R::roundRandomnessCall::SELECTOR,
                "roundRandomness(uint8,uint64)".to_owned(),
            ),
            (
                R::requestSeedCall::SELECTOR,
                "requestSeed(uint256)".to_owned(),
            ),
            (
                R::getProofContextCall::SELECTOR,
                "getProofContext(uint256,bytes)".to_owned(),
            ),
            (
                R::verifyRequestProofCall::SELECTOR,
                format!("verifyRequestProof(uint256,{proof})"),
            ),
            (
                R::fulfillRandomnessCall::SELECTOR,
                format!("fulfillRandomness(uint256,{proof},bytes)"),
            ),
            (
                R::fulfillRandomnessBatchCall::SELECTOR,
                format!("fulfillRandomnessBatch((uint8,uint64,bytes)[],uint256[],{proof}[])"),
            ),
        ] {
            assert_eq!(binding, selector(&signature), "{signature}");
        }
        for (binding, signature) in [
            (
                R::RoundAssigned::SIGNATURE_HASH,
                "RoundAssigned(uint256,uint8,uint64)",
            ),
            (
                R::RoundVerified::SIGNATURE_HASH,
                "RoundVerified(uint8,uint64,bytes32,bytes)",
            ),
            (
                R::KeeperChanged::SIGNATURE_HASH,
                "KeeperChanged(address,address)",
            ),
            (
                R::BackupKeeperSet::SIGNATURE_HASH,
                "BackupKeeperSet(address,bool)",
            ),
            (
                R::RandomnessRequested::SIGNATURE_HASH,
                "RandomnessRequested(uint256,address,bytes32,bytes32,uint64,uint32,uint256,address,uint64)",
            ),
        ] {
            assert_eq!(binding, keccak256(signature), "{signature}");
        }
    }

    /// A struct's canonical tuple, from its EIP-712 type: `Name(type a,type b)` is `(type,type)`.
    fn tuple_of(eip712: &str) -> String {
        let fields = &eip712[eip712.find('(').unwrap() + 1..eip712.len() - 1];
        let types: Vec<&str> = fields
            .split(',')
            .map(|field| field.split(' ').next().unwrap())
            .collect();
        format!("({})", types.join(","))
    }
    /// A type list as the binding names it, with each of its structs written out as its tuple.
    fn canonical(sol_name: &str) -> String {
        use alloy_sol_types::SolStruct;
        let mut name = sol_name.to_owned();
        for (structure, eip712) in [
            ("RoundRequest", RoundRequest::eip712_root_type()),
            ("RoundSignature", RoundSignature::eip712_root_type()),
            ("RoundProof", RoundProof::eip712_root_type()),
            ("Beacon", Beacon::eip712_root_type()),
        ] {
            name = name.replace(structure, &tuple_of(&eip712));
        }
        name
    }

    /// Every function and event of the binding is the compiled round coordinator's, exactly: its signature and selector
    /// or topic, what a function returns and which inputs of an event are indexed. The fixture is the compiled contract's
    /// ABI (`keeper/tests/fixtures/round-coordinator-abi.json`; its `regenerate` field says how to make it again).
    #[test]
    fn every_function_and_event_of_the_binding_is_the_compiled_round_coordinators() {
        use RoundCoordinator as R;
        use alloy_sol_types::{SolType, TopicList};
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/round-coordinator-abi.json"))
                .unwrap();
        let entry = |list: &str, signature: &str| -> serde_json::Value {
            fixture[list]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["signature"] == signature)
                .unwrap_or_else(|| panic!("the compiled coordinator has no {signature}"))
                .clone()
        };
        // Every function: its selector, and what it returns.
        let mut checked = Vec::new();
        macro_rules! function {
            ($($call:ty),* $(,)?) => {$(
                let signature = <$call as SolCall>::SIGNATURE;
                let compiled = entry("functions", signature);
                assert_eq!(
                    compiled["selector"],
                    format!("0x{}", hex::encode(<$call as SolCall>::SELECTOR)),
                    "{signature}"
                );
                assert_eq!(
                    compiled["outputs"],
                    canonical(<<$call as SolCall>::ReturnTuple<'static> as SolType>::SOL_NAME),
                    "{signature}"
                );
                checked.push(<$call as SolCall>::SELECTOR);
            )*};
        }
        function!(
            R::nextRequestIdCall,
            R::getPendingRequestIdsCall,
            R::getRoundRequestCall,
            R::requestFeePaidCall,
            R::publicKeyXCall,
            R::publicKeyYCall,
            R::keyHashCall,
            R::protocolConfigurationHashCall,
            R::pricingCall,
            R::keeperFeeBpsCall,
            R::feeRecipientCall,
            R::keeperCall,
            R::isBackupKeeperCall,
            R::isAuthorizedKeeperCall,
            R::backupKeeperCountCall,
            R::ROUND_LEADCall,
            R::beaconCountCall,
            R::getBeaconCall,
            R::beaconScheduleCall,
            R::roundAtCall,
            R::roundTimeCall,
            R::roundRandomnessCall,
            R::checkRoundSignatureCall,
            R::ROUND_BEACON_DOMAINCall,
            R::ROUND_VERIFY_GASCall,
            R::requestSeedCall,
            R::getProofContextCall,
            R::verifyRequestProofCall,
            R::fulfillRandomnessCall,
            R::fulfillRandomnessBatchCall,
        );
        // The list above is the whole binding.
        checked.sort_unstable();
        let mut bound = R::RoundCoordinatorCalls::SELECTORS.to_vec();
        bound.sort_unstable();
        assert_eq!(checked, bound);
        // Every event: its topic, and how many of its inputs are indexed.
        let mut checked = Vec::new();
        macro_rules! event {
            ($($event:ty),* $(,)?) => {$(
                let signature = <$event as SolEvent>::SIGNATURE;
                let compiled = entry("events", signature);
                assert_eq!(
                    compiled["topic"],
                    <$event as SolEvent>::SIGNATURE_HASH.to_string(),
                    "{signature}"
                );
                assert_eq!(
                    compiled["indexed"].as_u64(),
                    Some((<<$event as SolEvent>::TopicList as TopicList>::COUNT - 1) as u64),
                    "{signature}"
                );
                assert_eq!(compiled["anonymous"], false, "{signature}");
                checked.push(<$event as SolEvent>::SIGNATURE_HASH);
            )*};
        }
        event!(
            R::RandomnessRequested,
            R::RoundAssigned,
            R::RoundVerified,
            R::RandomnessFulfilled,
            R::RequestServed,
            R::FulfillmentSkipped,
            R::KeeperChanged,
            R::BackupKeeperSet,
        );
        checked.sort_unstable();
        let mut bound: Vec<_> = R::RoundCoordinatorEvents::SELECTORS
            .iter()
            .map(|topic| alloy_primitives::B256::from(*topic))
            .collect();
        bound.sort_unstable();
        assert_eq!(checked, bound);
        // A request the coordinator does not have is refused with UnknownRequest, which a keeper reads as a request that
        // vanished from the chain.
        entry("errors", "UnknownRequest()");
    }

    /// No function of the binding is one of the epoch coordinator's functions that a round coordinator does not have:
    /// `getRequest`, `epochRegistry`, `confirmationBlocks` and the one-argument `getProofContext`.
    #[test]
    fn the_binding_has_none_of_the_epoch_coordinators_own_functions() {
        use crate::abi::Coordinator as C;
        let epoch_only = [
            C::getRequestCall::SELECTOR,
            C::epochRegistryCall::SELECTOR,
            C::confirmationBlocksCall::SELECTOR,
            C::getProofContextCall::SELECTOR,
            C::fulfillRandomnessCall::SELECTOR,
            C::fulfillRandomnessBatchCall::SELECTOR,
        ];
        for round in RoundCoordinator::RoundCoordinatorCalls::SELECTORS {
            assert!(!epoch_only.contains(round), "0x{}", hex::encode(round));
        }
    }

    /// No function of the binding has the selector of an epoch registry's function, so that a keeper's calls are told
    /// apart from a registry's by their selectors alone. The beacon book's reads were `beaconOf` and `verifyBeacon`,
    /// the registry's names, before the round coordinator took its own.
    #[test]
    fn the_binding_shares_no_selector_with_an_epoch_registry() {
        use crate::abi::EpochRegistry as E;
        for round in RoundCoordinator::RoundCoordinatorCalls::SELECTORS {
            assert!(
                !E::EpochRegistryCalls::SELECTORS.contains(round),
                "0x{}",
                hex::encode(round)
            );
        }
        for retired in [
            "beaconOf(uint8)",
            "verifyBeacon(uint8,uint64,bytes)",
            "BEACON_DOMAIN()",
            "BEACON_VERIFY_GAS()",
        ] {
            assert!(
                !RoundCoordinator::RoundCoordinatorCalls::SELECTORS.contains(&selector(retired)),
                "{retired}"
            );
        }
    }
}
