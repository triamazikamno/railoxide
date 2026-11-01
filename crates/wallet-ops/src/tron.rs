//! Public Tron accounts: addresses, transactions and fees. Tron is not an EVM chain and never
//! enters the chain registry.

pub mod address;
pub mod balances;
pub mod confirmation;
pub mod fees;
pub mod http_api;
mod route;
pub mod send;
pub mod transaction;

pub use railgun_ui::TRON_CHAIN_ID;
pub use route::tron_rpc_route;
