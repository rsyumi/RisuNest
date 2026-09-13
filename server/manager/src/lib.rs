pub mod client;
pub mod lifecycle;
pub mod platform;
pub mod terminal;
pub mod tui;

pub type Result<T> = std::result::Result<T, String>;
