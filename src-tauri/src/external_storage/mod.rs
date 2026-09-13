//! Provider-independent native external storage boundaries. No test provider is
//! compiled into the product; unimplemented services remain absent from the UI.
pub(crate) mod admission;
pub(crate) mod auth;
pub(crate) mod capabilities;
pub(crate) mod capture;
pub(crate) mod contract;
pub(crate) mod durable_quota;
#[cfg(test)]
pub(crate) mod fake;
pub(crate) mod http;
#[cfg(test)]
mod http_tests;
pub(crate) mod providers;
pub(crate) mod publication;
pub(crate) mod quota;
pub(crate) mod registry;
#[cfg(test)]
mod tests;
pub(crate) mod transfer;
#[cfg(test)]
pub(crate) mod wire_fixture;
