use alloy::sol;

sol! {
    struct RelayAdaptCall {
        address to;
        bytes data;
        uint256 value;
    }

    interface PublicRelayAdapt {
        function multicall(bool _requireSuccess, RelayAdaptCall[] _calls) external payable;
        function wrapBase(uint256 _amount) external;
    }

    interface PublicErc20 {
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function transfer(address recipient, uint256 amount) external returns (bool);
    }
}
