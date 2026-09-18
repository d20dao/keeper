// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {DataTemplate} from "../libraries/DataTemplate.sol";

/// @dev Test-only: the registry's DataTemplate interpreter, with batched verdicts for differential tests.
contract DataTemplateHarness {
    function isValid(bytes calldata template) external pure returns (bool) {
        return DataTemplate.isValid(template);
    }

    function validity(bytes[] calldata templates) external pure returns (bool[] memory result) {
        result = new bool[](templates.length);
        for (uint256 i; i < templates.length; ++i) result[i] = DataTemplate.isValid(templates[i]);
    }

    /// @dev Only for well-formed templates, as in the registry.
    function verdicts(bytes calldata template, bytes[] calldata records) external pure returns (bool[] memory result) {
        require(DataTemplate.isValid(template), "malformed template");
        bytes memory t = template;
        result = new bool[](records.length);
        for (uint256 i; i < records.length; ++i) result[i] = DataTemplate.matches(t, records[i]);
    }
}

/// @dev Test-only oracle: EpochEntropy's per-recipe validator as deployed on Arc Testnet at commit 96cc722, copied
/// verbatim. Recipe ids are that implementation's: 0 Hyperliquid BTC volume, 1 and 6 block hashes, 2 and 3 TickerLayer
/// trades, 4 and 7 Nodary feeds, 5 Hyperliquid SOL mid.
contract LegacyEpochDataValidator {
    error InvalidData();

    function verdicts(uint8 recipe, bytes[] calldata records) external view returns (bool[] memory result) {
        result = new bool[](records.length);
        for (uint256 i; i < records.length; ++i) {
            try this.validate(recipe, records[i]) { result[i] = true; } catch { result[i] = false; }
        }
    }

    function validate(uint8 recipe, bytes calldata data) external pure { _validate(recipe, data); }

    function _validate(uint8 recipe,bytes calldata data) private pure {
        if(data.length==0||data.length>128) revert InvalidData();
        uint256 p;
        if(recipe==0||recipe==5) {
            // Hyperliquid projections: a nonnegative decimal string.
            p=_literal(data,0,recipe==0?bytes('{"symbol":"BTC","value":"'):bytes('{"mid":"'));
            p=_number(data,p,true,false); p=_literal(data,p,bytes('"}'));
        } else if(recipe==1||recipe==6) {
            // The JSON-RPC envelope of one block hash: 64 lowercase hex characters.
            p=_literal(data,0,bytes('{"id":null,"jsonrpc":"2.0","result":"0x'));
            p=_hex(data,p,64); p=_literal(data,p,bytes('"}'));
        } else if(recipe==2||recipe==3) {
            p=_literal(data,0,recipe==2?bytes('{"symbol":"BTCUSD","price":'):bytes('{"symbol":"ETHUSD","price":'));
            p=_number(data,p,true,true); p=_literal(data,p,bytes(',"size":'));
            p=_number(data,p,true,true); p=_literal(data,p,bytes(',"timestamp":'));
            uint256 first=p; p=_number(data,p,false,false);
            if(data[first]==0x30||p-first>16) revert InvalidData();
            p=_literal(data,p,bytes('}'));
        } else if(recipe==4||recipe==7) {
            // Nodary feed: JSON number value, 13-digit millisecond timestamp, crypto category.
            p=_literal(data,0,recipe==4?bytes('{"ETH/USD":{"value":'):bytes('{"BTC/USD":{"value":'));
            p=_number(data,p,true,true); p=_literal(data,p,bytes(',"timestamp":'));
            uint256 first=p; p=_number(data,p,false,false);
            if(p-first!=13) revert InvalidData();
            p=_literal(data,p,bytes(',"category":"crypto"}}'));
        } else revert InvalidData();
        if(p!=data.length) revert InvalidData();
    }
    function _literal(bytes calldata data,uint256 p,bytes memory literal) private pure returns(uint256) {
        if(p+literal.length>data.length) revert InvalidData();
        for(uint256 i;i<literal.length;++i)if(data[p+i]!=literal[i])revert InvalidData();return p+literal.length;
    }
    function _hex(bytes calldata data,uint256 p,uint256 length) private pure returns(uint256 end) {
        end=p+length;
        if(end>data.length) revert InvalidData();
        for(;p<end;++p) { uint8 c=uint8(data[p]); if(!((c>=48&&c<=57)||(c>=97&&c<=102))) revert InvalidData(); }
    }
    function _digit(bytes1 c) private pure returns(bool){return c>=0x30&&c<=0x39;}
    /// @dev Unsigned JSON number: an integer without leading zeros, then an optional fraction and exponent when allowed.
    function _number(bytes calldata data,uint256 p,bool fraction,bool exponent) private pure returns(uint256) {
        if(p>=data.length||!_digit(data[p]))revert InvalidData();
        if(data[p]==0x30){++p;if(p<data.length&&_digit(data[p]))revert InvalidData();}
        else {while(p<data.length&&_digit(data[p]))++p;}
        if(fraction&&p<data.length&&data[p]==0x2e){++p;uint256 first=p;while(p<data.length&&_digit(data[p]))++p;if(p==first)revert InvalidData();}
        if(exponent&&p<data.length&&(data[p]==0x65||data[p]==0x45)){
            ++p;if(p<data.length&&(data[p]==0x2b||data[p]==0x2d))++p;
            uint256 first=p;while(p<data.length&&_digit(data[p]))++p;if(p==first)revert InvalidData();
        }
        return p;
    }
}

/// @dev Test-only oracle: the ANU quantum random numbers record (source 1) of EpochEntropy at commit 640b60c, copied verbatim.
contract LegacyAnuValidator {
    error InvalidData();

    function verdicts(bytes[] calldata records) external view returns (bool[] memory result) {
        result = new bool[](records.length);
        for (uint256 i; i < records.length; ++i) {
            try this.validate(records[i]) { result[i] = true; } catch { result[i] = false; }
        }
    }

    function validate(bytes calldata data) external pure {
        if(data.length==0||data.length>128) revert InvalidData();
        bytes memory shape=bytes('{"success":true,"type":"hex8","length":"4","data":["0000000000000000","0000000000000000","0000000000000000","0000000000000000"]}');
        if(data.length!=shape.length) revert InvalidData();
        for(uint256 i;i<shape.length;++i) {
            if(shape[i]==0x30) { uint8 c=uint8(data[i]); if(!((c>=48&&c<=57)||(c>=97&&c<=102))) revert InvalidData(); }
            else if(data[i]!=shape[i]) revert InvalidData();
        }
    }
}
