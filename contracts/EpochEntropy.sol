// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;
import {ECDSA} from "@openzeppelin/contracts/utils/cryptography/ECDSA.sol";
import {MessageHashUtils} from "@openzeppelin/contracts/utils/cryptography/MessageHashUtils.sol";
import {Ownable2StepUpgradeable} from "@openzeppelin/contracts-upgradeable/access/Ownable2StepUpgradeable.sol";
import {UUPSUpgradeable} from "@openzeppelin/contracts/proxy/utils/UUPSUpgradeable.sol";
import {DataTemplate} from "./libraries/DataTemplate.sol";

/// @notice Signed API recipes from an owner-managed, append-only registry, published on demand for each 200-block epoch.
contract EpochEntropy is Ownable2StepUpgradeable, UUPSUpgradeable {
    uint64 public constant EPOCH_LENGTH = 200;
    // Attestation freshness at publication; the keeper enforces the same bound before sending.
    uint256 public constant MAX_ATTESTATION_AGE = 240 seconds;
    uint256 public constant MAX_PACKET_BYTES = 2048;
    // Deterministic source fallback: attempt n commits the source n slots after the selected one, and only once
    // n × FALLBACK_DELAY_BLOCKS blocks of the epoch have passed. Randomness still binds a later target block hash.
    uint64 public constant FALLBACK_DELAY_BLOCKS = 20;
    // A catalog lists 1 to MAX_SOURCES distinct recipes, so its last fallback window opens inside the epoch.
    uint256 public constant MAX_SOURCES = 10;
    // Recipe ids are uint8 in catalogs, selections and events.
    uint256 public constant MAX_RECIPES = 256;
    // With these bounds every registered recipe's EpochCommitted packet fits MAX_PACKET_BYTES.
    uint256 public constant MAX_REQUEST_BYTES = 1024;
    uint256 public constant MAX_BODY_BYTES = 2048;
    uint256 public constant MAX_DATA_BYTES = DataTemplate.MAX_DATA_BYTES;
    uint256 public constant MAX_TEMPLATE_BYTES = DataTemplate.MAX_TEMPLATE_BYTES;
    // Backup committers publish exactly like the primary committer; a small bound keeps that set reviewable.
    uint256 public constant MAX_BACKUP_COMMITTERS = 4;
    bytes32 public constant RECIPE_DOMAIN = keccak256("D20_EPOCH_RECIPES");
    bytes32 public constant SELECT_DOMAIN = keccak256("D20_EPOCH_SELECT");
    bytes32 public constant EPOCH_DOMAIN = keccak256("D20_EPOCH");
    // Initial catalog: recipes 0-3 with these signers, bound into catalogHash. These getters never change;
    // the catalog in force for an epoch comes from catalogAt(epoch).
    address public hyperliquidSigner;
    /// @notice Initial-catalog signer of recipe 1, the Ethereum mainnet block hash.
    address public ethereumBlockSigner;
    address public btcTradeSigner;
    address public ethTradeSigner;
    address public committer;
    uint64 public firstEpochStart;
    bytes32 public catalogHash;
    struct Attestation { uint256 timestamp; bytes data; bytes signature; }
    /// @dev source is the slot in the epoch's catalog; recipe is the global recipe id at that slot.
    struct Selection { uint8 source; uint8 recipe; address airnode; bytes32 selector; bytes32 queryHash; string canonicalRequest; }
    struct Epoch {
        bytes32 epochHash; bytes32 catalogHash; bytes32 anchorHash; uint8 source;
        bytes32 queryHash; bytes32 dataHash; bytes32 attestationHash; uint256 signedAt; uint64 committedBlock;
    }
    struct Catalog { uint64 fromEpoch; bytes32 hash; uint8[] recipes; address[] signers; }
    /// @dev canonicalRequest hashes to the signed query, template is a DataTemplate and body is the JSON a keeper posts.
    struct Recipe { string canonicalRequest; bytes template; string body; }
    mapping(uint64 => Epoch) private epochs;
    mapping(uint64 => bytes32) public epochAnchors;
    // Length slot of the retired four-signer catalog array. It is zero on every deployed registry and stays unused.
    uint256 private __retiredCatalogVersions;
    // Scheduled catalogs ascending by fromEpoch; epochs before the first entry use the initial catalog.
    Catalog[] private catalogs;
    // Append-only recipe registry: an id is its index, and a registered recipe never changes.
    Recipe[] private registeredRecipes;
    // Owner-approved wallets that may publish epochs besides the committer, and that the coordinator pays the
    // keeper share of the requests they serve themselves.
    mapping(address => bool) private backupCommitters;
    uint256 public backupCommitterCount;
    // Preserve all declared fields/mapping value layouts; consume reserved slots when extending.
    uint256[35] private __gap;
    error InvalidConfig(); error InvalidEpoch(); error PreparationClosed(); error AnchorUnavailable();
    error OnlyCommitter(); error AlreadyCommitted(); error InvalidTime(); error InvalidData(); error InvalidSigner();
    error PacketTooLarge(); error RenounceDisabled();
    error InvalidFallback(); error FallbackNotOpen();
    error InvalidRecipe(); error InvalidTemplate();
    event EpochCommitted(uint64 indexed epochId, bytes32 indexed epochHash, bytes packet);
    event CommitterChanged(address indexed previousCommitter, address indexed newCommitter);
    event CatalogScheduled(uint64 indexed fromEpoch, bytes32 indexed catalogHash, uint8[] recipes, address[] signers);
    event RecipeRegistered(uint8 indexed recipe, bytes32 indexed queryHash, string canonicalRequest, bytes template, string body);
    event BackupCommitterSet(address indexed account, bool allowed);
    /// @custom:oz-upgrades-unsafe-allow constructor
    constructor() { _disableInitializers(); }
    function initialize(address[4] memory signers, address initialOwner, address initialCommitter) external initializer {
        __Ownable_init(initialOwner);
        __Ownable2Step_init();
        if(signers[0]==address(0)||signers[1]==address(0)||signers[2]==address(0)||signers[3]==address(0)||initialCommitter==address(0)) revert InvalidConfig();
        hyperliquidSigner=signers[0]; ethereumBlockSigner=signers[1]; btcTradeSigner=signers[2]; ethTradeSigner=signers[3]; committer=initialCommitter;
        firstEpochStart=uint64(block.number)+EPOCH_LENGTH;
        catalogHash=keccak256(abi.encode(RECIPE_DOMAIN,signers));
        _registerBuiltinRecipes();
    }
    /// @notice Upgrade step for a registry initialized before the recipe registry: registers built-in recipes 0-5,
    /// which its initial catalog selects from. Pass it as the upgradeToAndCall data so the upgrade is atomic.
    /// @dev Owner-only and once per proxy (reinitializer 2). It refuses a registry that already has recipes, which
    /// includes every proxy initialized by this implementation, and any registry with a catalog scheduled under
    /// the previous hardcoded recipe ids.
    function initializeRecipeRegistry() external reinitializer(2) onlyOwner {
        if(registeredRecipes.length!=0||catalogs.length!=0||__retiredCatalogVersions!=0) revert InvalidConfig();
        _registerBuiltinRecipes();
    }
    function _authorizeUpgrade(address) internal override onlyOwner {}
    /// @notice Upgrade authority can only move through the two-step transfer; it can never be abandoned.
    function renounceOwnership() public view override onlyOwner { revert RenounceDisabled(); }

    function setCommitter(address next) external onlyOwner {
        if(next==address(0)) revert InvalidConfig();
        emit CommitterChanged(committer,next); committer=next;
    }
    /// @notice Allow or remove a backup committer: a separate keeper wallet that may publish epochs with exactly the
    /// primary committer's rules, for example a follower keeper that takes over while the primary is down. It has no
    /// other role; the coordinator pays it the keeper share of the requests whose accepted proofs it submits.
    function setBackupCommitter(address account, bool allowed) external onlyOwner {
        if(account==address(0)||(allowed&&account==committer)||backupCommitters[account]==allowed) revert InvalidConfig();
        if(allowed) {
            if(backupCommitterCount==MAX_BACKUP_COMMITTERS) revert InvalidConfig();
            ++backupCommitterCount;
        } else {
            --backupCommitterCount;
        }
        backupCommitters[account]=allowed;
        emit BackupCommitterSet(account,allowed);
    }
    function isBackupCommitter(address account) external view returns(bool) { return backupCommitters[account]; }
    /// @notice Whether an account may publish epochs: the committer or an allowed backup committer. The coordinator
    /// reads this to pay the keeper share to the wallet that submitted an accepted proof.
    function isAuthorizedCommitter(address account) external view returns(bool) { return _authorized(account); }
    function _authorized(address account) private view returns(bool) { return account==committer||backupCommitters[account]; }
    /// @notice Append an immutable recipe. It can never be edited or removed, so a changed listing becomes a new id.
    /// @param canonicalRequest AirnodeHub canonical request; its keccak256 is the query hash the signer signs.
    /// @param template DataTemplate of the exact signed data the recipe accepts.
    /// @param body JSON request body a keeper posts to the provider's gateway. Keepers refuse a body that does not
    /// canonicalize to canonicalRequest; the contract only bounds its size.
    function registerRecipe(string memory canonicalRequest, bytes memory template, string memory body) external onlyOwner returns(uint8 recipe) {
        return _registerRecipe(canonicalRequest,template,body);
    }
    function recipeCount() external view returns(uint256) { return registeredRecipes.length; }
    /// @notice A registered recipe: its query hash, canonical request, data template and gateway request body.
    function getRecipe(uint8 recipe) external view returns(bytes32 queryHash, string memory canonicalRequest, bytes memory template, string memory body) {
        Recipe storage r=_recipe(recipe);
        return (keccak256(bytes(r.canonicalRequest)),r.canonicalRequest,r.template,r.body);
    }
    /// @notice AirnodeHub canonical request of a recipe: objects sorted by key at every depth, arrays in order.
    function recipeRequest(uint8 recipe) external view returns(string memory) { return _recipe(recipe).canonicalRequest; }
    function _recipe(uint8 recipe) private view returns(Recipe storage) {
        if(recipe>=registeredRecipes.length) revert InvalidConfig();
        return registeredRecipes[recipe];
    }
    function _registerRecipe(string memory canonicalRequest, bytes memory template, string memory body) private returns(uint8 recipe) {
        uint256 id=registeredRecipes.length;
        uint256 requestBytes=bytes(canonicalRequest).length;
        uint256 bodyBytes=bytes(body).length;
        if(id>=MAX_RECIPES||requestBytes==0||requestBytes>MAX_REQUEST_BYTES||bodyBytes==0||bodyBytes>MAX_BODY_BYTES) revert InvalidRecipe();
        if(!DataTemplate.isValid(template)) revert InvalidTemplate();
        registeredRecipes.push(Recipe(canonicalRequest,template,body));
        recipe=uint8(id);
        emit RecipeRegistered(recipe,keccak256(bytes(canonicalRequest)),canonicalRequest,template,body);
    }
    /// @dev Ids 0-3 keep the canonical requests of the previous hardcoded recipes, so the initial catalog and every
    /// epoch committed before the registry select, commit and replay unchanged.
    function _registerBuiltinRecipes() private {
        // 0: Hyperliquid BTC daily notional volume, a decimal string.
        _registerRecipe(
            '["metaAndAssetCtxs",[["dex",""]],[["symbol","/0/universe/0/name"],["value","/1/0/dayNtlVlm"]]]',
            bytes.concat(DataTemplate.literal('{"symbol":"BTC","value":"'),DataTemplate.decimal(true,false),DataTemplate.literal('"}')),
            '{"operation":"metaAndAssetCtxs","parameters":{"dex":""},"responseProjection":{"symbol":"/0/universe/0/name","value":"/1/0/dayNtlVlm"}}');
        _registerBlockHashRecipe("ethereum");
        _registerTradeRecipe("BTCUSD");
        _registerTradeRecipe("ETHUSD");
        // 4: Nodary ETH/USD feed, served first-party: JSON number value, 13-digit millisecond timestamp.
        _registerRecipe(
            '["latestFeeds",[["name","ETH/USD"]]]',
            bytes.concat(DataTemplate.literal('{"ETH/USD":{"value":'),DataTemplate.decimal(true,true),DataTemplate.literal(',"timestamp":'),
                DataTemplate.integer(13,13),DataTemplate.literal(',"category":"crypto"}}')),
            '{"operation":"latestFeeds","parameters":{"name":"ETH/USD"}}');
        _registerBlockHashRecipe("base");
    }
    /// @dev 1 and 5: dRPC eth_call of Multicall3 getLastBlockHash() at "latest", one block hash in its JSON-RPC envelope.
    function _registerBlockHashRecipe(string memory network) private {
        _registerRecipe(
            string.concat('["jsonRpc",[["method","eth_call"],["network","',network,
                '"],["params",[[["data","0x27e86d6e"],["to","0xcA11bde05977b3631167028862bE2a173976CA11"]],"latest"]]]]'),
            bytes.concat(DataTemplate.literal('{"id":null,"jsonrpc":"2.0","result":"0x'),DataTemplate.hexChars(64),DataTemplate.literal('"}')),
            string.concat('{"operation":"jsonRpc","parameters":{"network":"',network,
                '","method":"eth_call","params":[{"to":"0xcA11bde05977b3631167028862bE2a173976CA11","data":"0x27e86d6e"},"latest"]}}'));
    }
    /// @dev 2 and 3: TickerLayer crypto last trade, JSON number price and size, timestamp of at most 16 digits.
    function _registerTradeRecipe(string memory symbol) private {
        _registerRecipe(
            string.concat('["lastTrade",[["assetClass","crypto"],["symbol","',symbol,'"]]]'),
            bytes.concat(DataTemplate.literal(string.concat('{"symbol":"',symbol,'","price":')),DataTemplate.decimal(true,true),
                DataTemplate.literal(',"size":'),DataTemplate.decimal(true,true),
                bytes.concat(DataTemplate.literal(',"timestamp":'),DataTemplate.integer(1,16),DataTemplate.literal('}'))),
            string.concat('{"operation":"lastTrade","parameters":{"assetClass":"crypto","symbol":"',symbol,'"}}'));
    }
    /// @notice Schedule a catalog of distinct registered recipes and their signers for epochs >= fromEpoch, at least
    /// two epochs ahead; a pending version is replaced.
    /// @dev catalogHash() and the initial signer getters never change, so protocolConfigurationHash and keeper pins never move.
    function scheduleCatalog(uint8[] calldata recipes, address[] calldata signers, uint64 fromEpoch) external onlyOwner {
        uint256 count=recipes.length;
        uint256 registered=registeredRecipes.length;
        if(count==0||count>MAX_SOURCES||signers.length!=count) revert InvalidConfig();
        uint256 seen;
        for(uint256 i;i<count;++i) {
            uint8 recipe=recipes[i];
            if(recipe>=registered||seen&(1<<recipe)!=0||signers[i]==address(0)) revert InvalidConfig();
            seen|=1<<recipe;
        }
        uint64 current=epochForBlock(block.number);
        if(fromEpoch<current+2) revert InvalidEpoch();
        uint256 versions=catalogs.length;
        if(versions!=0&&catalogs[versions-1].fromEpoch>current) catalogs.pop();
        bytes32 hash=keccak256(abi.encode(RECIPE_DOMAIN,recipes,signers));
        Catalog storage c=catalogs.push();
        c.fromEpoch=fromEpoch; c.hash=hash; c.recipes=recipes; c.signers=signers;
        emit CatalogScheduled(fromEpoch,hash,recipes,signers);
    }
    /// @notice The catalog an epoch selects and commits with: its hash and the recipe and signer of each slot.
    function catalogAt(uint64 epochId) public view returns(bytes32 hash, uint8[] memory recipes, address[] memory signers) {
        uint256 version=_versionAt(epochId);
        if(version!=0) { Catalog storage c=catalogs[version-1]; return (c.hash,c.recipes,c.signers); }
        recipes=new uint8[](4); signers=new address[](4);
        (recipes[1],recipes[2],recipes[3])=(1,2,3);
        (signers[0],signers[1],signers[2],signers[3])=(hyperliquidSigner,ethereumBlockSigner,btcTradeSigner,ethTradeSigner);
        hash=catalogHash;
    }
    /// @notice Number of sources, and so of selection attempts (0 to count-1), for an epoch.
    function sourceCountAt(uint64 epochId) public view returns(uint256) {
        uint256 version=_versionAt(epochId);
        return version==0?4:catalogs[version-1].recipes.length;
    }
    /// @dev One plus the index of the scheduled catalog in force for an epoch; zero selects the initial catalog.
    function _versionAt(uint64 epochId) private view returns(uint256) {
        for(uint256 i=catalogs.length;i>0;--i) if(epochId>=catalogs[i-1].fromEpoch) return i;
        return 0;
    }
    function epochStart(uint64 epochId) public view returns(uint64) {
        if(epochId==0) revert InvalidEpoch();
        return firstEpochStart+(epochId-1)*EPOCH_LENGTH;
    }
    function epochForBlock(uint256 number) public view returns(uint64) {
        return number<firstEpochStart?0:uint64(1+(number-firstEpochStart)/EPOCH_LENGTH);
    }
    function nextEpochToPrepare(uint256 number) external view returns(uint64) { return epochForBlock(number); }
    /// @notice Preserve the canonical source selector without publishing any API data.
    /// @dev Requests call this while their epoch's start-1 block is within BLOCKHASH range.
    function checkpointEpoch(uint64 epochId) public returns(bytes32 anchor) {
        anchor=_anchor(epochId);
        if(epochAnchors[epochId]==bytes32(0)) epochAnchors[epochId]=anchor;
    }
    function _anchor(uint64 epochId) private view returns(bytes32 anchor) {
        uint64 start=epochStart(epochId);
        if(block.number<start) revert PreparationClosed();
        anchor=epochAnchors[epochId];
        if(anchor==bytes32(0)) anchor=blockhash(start-1);
        if(anchor==bytes32(0)) revert AnchorUnavailable();
    }
    function getEpoch(uint64 epochId) external view returns(Epoch memory) { return epochs[epochId]; }
    function getEpochSelection(uint64 epochId) external view returns(Selection memory s) { (s,)=_select(epochId,0); }
    /// @notice Source, recipe, signer and query for a fallback attempt (0 is the selected source).
    function getEpochFallbackSelection(uint64 epochId,uint8 attempt) external view returns(Selection memory s) { (s,)=_select(epochId,attempt); }
    /// @notice First block at which an attempt may be committed; attempt 0 opens at the epoch start.
    function fallbackOpensAt(uint64 epochId,uint8 attempt) public view returns(uint64) {
        if(attempt>=sourceCountAt(epochId)) revert InvalidFallback();
        return epochStart(epochId)+uint64(attempt)*FALLBACK_DELAY_BLOCKS;
    }
    /// @dev Selection and commitment use the catalog in force for the epoch being selected, not the initial one.
    function _select(uint64 epochId,uint8 attempt) private view returns(Selection memory s,bytes32 catalog) {
        bytes32 anchor=_anchor(epochId);
        uint8[] memory recipes; address[] memory signers;
        (catalog,recipes,signers)=catalogAt(epochId);
        if(attempt>=recipes.length) revert InvalidFallback();
        s.selector=keccak256(abi.encode(SELECT_DOMAIN,catalog,epochId,anchor));
        s.source=uint8((uint256(s.selector)%recipes.length+attempt)%recipes.length);
        s.recipe=recipes[s.source];
        s.airnode=signers[s.source];
        s.canonicalRequest=_recipe(s.recipe).canonicalRequest;
        s.queryHash=keccak256(bytes(s.canonicalRequest));
    }
    function commitEpoch(uint64 epochId, Attestation calldata a) external { _commit(epochId,0,a); }
    /// @notice Publish the source attempt slots after the selected one once its fallback window is open.
    function commitEpochFallback(uint64 epochId, uint8 attempt, Attestation calldata a) external {
        if(attempt==0) revert InvalidFallback();
        _commit(epochId,attempt,a);
    }
    function _commit(uint64 epochId, uint8 attempt, Attestation calldata a) private {
        if(!_authorized(msg.sender)) revert OnlyCommitter();
        if(epochs[epochId].epochHash!=bytes32(0)) revert AlreadyCommitted();
        if(block.number<fallbackOpensAt(epochId,attempt)) revert FallbackNotOpen();
        (Selection memory s,bytes32 catalog)=_select(epochId,attempt);
        if(a.timestamp>block.timestamp||block.timestamp-a.timestamp>MAX_ATTESTATION_AGE) revert InvalidTime();
        // Only the recipe's exact signed record: its template fixes literals, key order and number grammar.
        if(!DataTemplate.matches(registeredRecipes[s.recipe].template,a.data)) revert InvalidData();
        bytes32 digest=keccak256(abi.encodePacked(s.queryHash,a.timestamp,a.data));
        if(ECDSA.recover(MessageHashUtils.toEthSignedMessageHash(digest),a.signature)!=s.airnode) revert InvalidSigner();
        bytes32 dataHash=keccak256(a.data);
        bytes32 attestationHash=keccak256(abi.encode(s.queryHash,a.timestamp,dataHash,keccak256(a.signature)));
        bytes32 anchor=checkpointEpoch(epochId);
        // Request randomness always uses a block strictly after this commitment.
        bytes32 commitment=keccak256(abi.encode(EPOCH_DOMAIN,block.chainid,address(this),catalog,epochId,
            epochStart(epochId),anchor,s.source,s.queryHash,dataHash,attestationHash));
        epochs[epochId]=Epoch(commitment,catalog,anchor,s.source,s.queryHash,dataHash,attestationHash,a.timestamp,uint64(block.number));
        bytes memory packet=abi.encode(s.canonicalRequest,a);
        if(packet.length>MAX_PACKET_BYTES) revert PacketTooLarge();
        emit EpochCommitted(epochId,commitment,packet);
    }
}
