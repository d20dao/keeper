use alloy_sol_types::sol;
sol! {
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    struct VrfProof {
        uint256[2] pk; uint256[2] gamma; uint256 c; uint256 s; uint256 seed; address uWitness;
        uint256[2] cGammaWitness; uint256[2] sHashWitness; uint256 zInv;
    }
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    struct ApiProof { uint256 timestamp; bytes data; bytes signature; }
    #[derive(Debug, Default)]
    struct Request {
        address consumer; uint32 callbackGasLimit; uint64 requestBlock; uint64 targetBlock; uint64 deadline; address refundAddress;
        bytes32 clientSeed; bytes32 mappingHash; bytes32 blockHash; bytes32 randomness; bytes32 proofHash;
        bytes32 transcriptHash; bool fulfilled; bool delivered; bool refunded; uint64 epochId; bytes32 epochHash;
    }
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    struct EpochSelection { uint8 source; uint8 recipe; address airnode; bytes32 selector; bytes32 queryHash; string canonicalRequest; }
    #[derive(Debug)]
    struct EpochRecord {
        bytes32 epochHash; bytes32 catalogHash; bytes32 anchorHash; uint8 source;
        bytes32 queryHash; bytes32 dataHash; bytes32 attestationHash; uint256 signedAt; uint64 committedBlock;
    }
    interface EpochRegistry {
        function committer() external view returns (address owner);
        function isBackupCommitter(address account) external view returns (bool allowed);
        function catalogHash() external view returns (bytes32 hash);
        function nextEpochToPrepare(uint256 number) external view returns (uint64 epochId);
        function epochStart(uint64 epochId) external view returns (uint64 number);
        function sourceCountAt(uint64 epochId) external view returns (uint256 count);
        function catalogAt(uint64 epochId) external view returns (bytes32 hash, uint8[] recipes, address[] signers);
        function getEpoch(uint64 epochId) external view returns (EpochRecord record);
        function getEpochFallbackSelection(uint64 epochId, uint8 attempt) external view returns (EpochSelection selection);
        function getRecipe(uint8 recipe) external view returns (bytes32 queryHash, string canonicalRequest, bytes template, string body);
        function commitEpoch(uint64 epochId, ApiProof attestation) external;
        function commitEpochFallback(uint64 epochId, uint8 attempt, ApiProof attestation) external;
    }
    interface Coordinator {
        function getRequest(uint256 id) external view returns (Request request);
        function epochRegistry() external view returns (address registry);
        function protocolConfigurationHash() external view returns (bytes32 hash);
        function getProofContext(uint256 id) external view returns (uint256 seed, uint64 deadline, bool fulfilled, bool refunded);
        function fulfillRandomness(uint256 id, VrfProof proof) external;
        function fulfillRandomnessBatch(uint256[] ids, VrfProof[] proofs) external;
        function requestFeePaid(uint256 requestId) external view returns (uint256 fee);
        function feeRecipient() external view returns (address recipient);
        event RandomnessFulfilled(uint256 indexed requestId, bytes32 randomness, address indexed submitter);
        /// Reason: 1 already fulfilled, 2 refunded, 3 past deadline. The member changes no state.
        event FulfillmentSkipped(uint256 indexed requestId, uint8 reason);
        function getPendingRequestIds(uint256 fromId, uint256 limit) external view returns (uint256[] ids, uint256 nextCursor);
        function nextRequestId() external view returns (uint256 count);
        function publicKeyX() external view returns (uint256 x);
        function publicKeyY() external view returns (uint256 y);
    }
}
