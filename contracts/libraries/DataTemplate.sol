// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

/// @notice The exact signed-data grammar of an epoch recipe, interpreted from a short byte template.
/// @dev A template is a sequence of segments, each an opcode byte followed by its operands:
///   0x01 LITERAL len bytes[len]   exactly these bytes (1 <= len <= 128)
///   0x02 HEX     n                exactly n characters 0-9 or a-f (1 <= n <= 128)
///   0x03 DECIMAL flags            an unsigned JSON number: 0 or a nonzero digit followed by digits, then when
///                                 flags & 1 an optional fraction "." digits+, and when flags & 2 an optional
///                                 exponent ("e" | "E") ("+" | "-")? digits+ (flags <= 3)
///   0x04 INTEGER min max          a nonzero digit followed by digits, min <= digit count <= max (1 <= min <= max <= 128)
/// Variable segments are greedy and never backtrack; data matches when the segments consume it exactly.
/// A well-formed template is at most 256 bytes, has at least one variable segment, and its shortest possible
/// match fits the 128-byte data limit. The replay library and the keeper implement the same verdicts.
library DataTemplate {
    uint256 internal constant MAX_DATA_BYTES = 128;
    uint256 internal constant MAX_TEMPLATE_BYTES = 256;
    uint8 internal constant LITERAL = 0x01;
    uint8 internal constant HEX = 0x02;
    uint8 internal constant DECIMAL = 0x03;
    uint8 internal constant INTEGER = 0x04;
    uint8 private constant FRACTION = 0x01;
    uint8 private constant EXPONENT = 0x02;

    /// @notice Whether a template is well-formed.
    function isValid(bytes memory template) internal pure returns (bool) {
        uint256 length = template.length;
        if (length == 0 || length > MAX_TEMPLATE_BYTES) return false;
        uint256 t;
        uint256 shortest;
        bool variable;
        while (t < length) {
            uint8 op = uint8(template[t]);
            if (op == LITERAL) {
                if (t + 1 >= length) return false;
                uint256 n = uint8(template[t + 1]);
                if (n == 0 || n > MAX_DATA_BYTES || t + 2 + n > length) return false;
                shortest += n;
                t += 2 + n;
            } else if (op == HEX) {
                if (t + 1 >= length) return false;
                uint256 n = uint8(template[t + 1]);
                if (n == 0 || n > MAX_DATA_BYTES) return false;
                shortest += n;
                t += 2;
                variable = true;
            } else if (op == DECIMAL) {
                if (t + 1 >= length || uint8(template[t + 1]) > (FRACTION | EXPONENT)) return false;
                shortest += 1;
                t += 2;
                variable = true;
            } else if (op == INTEGER) {
                if (t + 2 >= length) return false;
                uint256 min = uint8(template[t + 1]);
                uint256 max = uint8(template[t + 2]);
                if (min == 0 || min > max || max > MAX_DATA_BYTES) return false;
                shortest += min;
                t += 3;
                variable = true;
            } else {
                return false;
            }
        }
        return variable && shortest <= MAX_DATA_BYTES;
    }

    /// @notice Whether data is exactly a record the template describes. The template must be well-formed.
    /// @dev Unchecked arithmetic is safe: offsets stay below 257 because data and template lengths are bounded.
    function matches(bytes memory template, bytes calldata data) internal pure returns (bool) {
        uint256 length = data.length;
        if (length == 0 || length > MAX_DATA_BYTES) return false;
        uint256 p;
        uint256 t;
        bool ok = true;
        unchecked {
            while (ok && t < template.length) {
                uint8 op = uint8(template[t]);
                if (op == LITERAL) {
                    uint256 n = uint8(template[t + 1]);
                    if (p + n > length) return false;
                    for (uint256 i; i < n; ++i) if (data[p + i] != template[t + 2 + i]) return false;
                    p += n;
                    t += 2 + n;
                } else if (op == HEX) {
                    (ok, p) = _hex(data, p, uint8(template[t + 1]));
                    t += 2;
                } else if (op == DECIMAL) {
                    uint8 flags = uint8(template[t + 1]);
                    (ok, p) = _decimal(data, p, flags & FRACTION != 0, flags & EXPONENT != 0);
                    t += 2;
                } else if (op == INTEGER) {
                    (ok, p) = _integer(data, p, uint8(template[t + 1]), uint8(template[t + 2]));
                    t += 3;
                } else {
                    return false;
                }
            }
        }
        return ok && p == length;
    }

    /// @notice Segment encoders, for building templates in code.
    function literal(string memory text) internal pure returns (bytes memory) {
        return abi.encodePacked(LITERAL, uint8(bytes(text).length), text);
    }

    function hexChars(uint8 n) internal pure returns (bytes memory) {
        return abi.encodePacked(HEX, n);
    }

    function decimal(bool fraction, bool exponent) internal pure returns (bytes memory) {
        return abi.encodePacked(DECIMAL, (fraction ? FRACTION : 0) | (exponent ? EXPONENT : 0));
    }

    function integer(uint8 minDigits, uint8 maxDigits) internal pure returns (bytes memory) {
        return abi.encodePacked(INTEGER, minDigits, maxDigits);
    }

    function _hex(bytes calldata data, uint256 p, uint256 n) private pure returns (bool, uint256) {
        unchecked {
            uint256 end = p + n;
            if (end > data.length) return (false, p);
            for (; p < end; ++p) {
                bytes1 c = data[p];
                if (!_isDigit(c) && (c < 0x61 || c > 0x66)) return (false, p);
            }
            return (true, end);
        }
    }

    function _decimal(bytes calldata data, uint256 p, bool fraction, bool exponent) private pure returns (bool, uint256) {
        uint256 length = data.length;
        if (p >= length || !_isDigit(data[p])) return (false, p);
        unchecked {
            if (data[p] == 0x30) {
                ++p;
                if (p < length && _isDigit(data[p])) return (false, p);
            } else {
                p = _digits(data, p);
            }
            if (fraction && p < length && data[p] == 0x2e) {
                uint256 first = p + 1;
                p = _digits(data, first);
                if (p == first) return (false, p);
            }
            if (exponent && p < length && (data[p] == 0x65 || data[p] == 0x45)) {
                ++p;
                if (p < length && (data[p] == 0x2b || data[p] == 0x2d)) ++p;
                uint256 first = p;
                p = _digits(data, first);
                if (p == first) return (false, p);
            }
        }
        return (true, p);
    }

    function _integer(bytes calldata data, uint256 p, uint256 min, uint256 max) private pure returns (bool, uint256) {
        if (p >= data.length || !_isDigit(data[p]) || data[p] == 0x30) return (false, p);
        uint256 end = _digits(data, p);
        uint256 count;
        unchecked { count = end - p; }
        return (count >= min && count <= max, end);
    }

    function _digits(bytes calldata data, uint256 p) private pure returns (uint256) {
        unchecked {
            while (p < data.length && _isDigit(data[p])) ++p;
        }
        return p;
    }

    function _isDigit(bytes1 c) private pure returns (bool) {
        return c >= 0x30 && c <= 0x39;
    }
}
