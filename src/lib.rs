//! lmdeck: a deck of local LLM recipes for llama.cpp, vLLM, SGLang, LMDeploy,
//! TabbyAPI (exllamav3), LM Studio, MLX and TensorFold, plus wrapped
//! `start.sh` / `stop.sh` repos.

pub mod catalog;
pub mod checkout;
pub mod compat;
pub mod config;
pub mod deck;
pub mod engine;
pub mod hardware;
pub mod hosts;
pub mod models;
pub mod paths;
pub mod recipe;
pub mod runner;
pub mod sources;
pub mod telemetry;
pub mod theme;
pub mod tui;
pub mod ui;
pub mod updates;
pub mod util;
pub mod web;
pub mod wrap;
