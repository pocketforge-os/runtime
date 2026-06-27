//! The two interchangeable [`crate::backend::Backend`] implementations behind the ONE facade.

mod broker_client;
mod inproc;

pub use broker_client::BrokerClientBackend;
pub use inproc::InProcessBackend;
