#![forbid(unsafe_code)]

pub mod config;
pub mod ekctl;
pub mod health;
pub mod model;
pub mod policy;
pub mod request;
pub mod server;

#[cfg(test)]
mod fake_ekctl;
