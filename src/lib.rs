#![forbid(unsafe_code)]

pub mod auth;
pub mod config;
pub mod ekctl;
pub mod executable;
pub mod health;
pub mod mail;
pub mod model;
pub mod policy;
pub mod remindctl;
pub mod reminders_model;
pub mod request;
pub mod server;
pub mod service;
pub mod subprocess;

#[cfg(test)]
mod fake_ekctl;
