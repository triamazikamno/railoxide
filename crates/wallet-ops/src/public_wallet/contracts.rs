use alloy::sol;

sol! {
    interface PublicWrappedNative {
        function deposit() external payable;
        function withdraw(uint256 amount) external;
    }

    interface PublicErc20 {
        event Approval(address indexed owner, address indexed spender, uint256 value);
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function transfer(address recipient, uint256 amount) external returns (bool);
    }
}
