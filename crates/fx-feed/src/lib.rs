//! Keyless market data and local storage. No account, no API key.

pub mod store;
pub mod yahoo;

pub use store::Store;
