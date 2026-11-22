//! Small Polymarket client library behind the `pm` CLI.
//!
//! | Module | API | Used for |
//! |---|---|---|
//! | [`clob`] | CLOB (`clob.polymarket.com`) | L1/L2 auth, live prediction orders |
//! | [`data_api`] | Data API v2 | prediction positions |
//! | [`gamma`] | Gamma | market questions, search |
//! | [`perps`] | Perps (`api.perpetuals.polymarket.com`) | instruments, portfolio, orders, signed TP/SL |

pub mod clob;
pub mod config;
pub mod data_api;
pub mod eth;
pub mod gamma;
pub mod http;
pub mod perps;
