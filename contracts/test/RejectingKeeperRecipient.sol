// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

interface IKeeperCredit {
    function withdrawKeeperCredit(address payable recipient) external;
}

/// @dev Local test helper; never an operator account.
contract RejectingKeeperRecipient {
    address public immutable owner = msg.sender;
    bool public acceptsPayments;
    function setAcceptsPayments(bool value) external {
        require(msg.sender == owner);
        acceptsPayments = value;
    }
    function withdraw(address coordinator, address payable recipient) external {
        require(msg.sender == owner);
        IKeeperCredit(coordinator).withdrawKeeperCredit(recipient);
    }
    receive() external payable { require(acceptsPayments, "reject keeper payment"); }
}
