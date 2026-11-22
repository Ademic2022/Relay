//! Session logic shared by Host and Controller. Contains no platform code:
//! everything runs against the traits in relay-capture / relay-codec /
//! relay-input, so it is fully testable with mocks (§11).

pub mod abr;
pub mod auth_limit;
pub mod host;
pub mod idr;
pub mod latest;
