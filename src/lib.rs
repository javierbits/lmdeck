//! lmdeck: a deck of local LLM recipes for llama.cpp, vLLM, SGLang, LMDeploy,
//! TabbyAPI (exllamav3), LM Studio, MLX and TensorFold, plus wrapped
//! `start.sh` / `stop.sh` repos.

pub mod browse;
pub mod catalog;
pub mod checkout;
pub mod cluster;
pub mod compat;
pub mod config;
pub mod deck;
pub mod engine;
pub mod fabric;
pub mod hardware;
pub mod harness;
pub mod hosts;
pub mod hub;
pub mod icons;
pub mod index;
pub mod models;
pub mod ops;
pub mod paths;
pub mod peers;
pub mod pick;

pub mod peerdeck;
pub mod placement;
pub mod recipe;
pub mod remote;
pub mod runner;
pub mod schema;
pub mod setup;
pub mod sources;
pub mod ssh;

pub mod telemetry;
pub mod theme;
pub mod topology;
pub mod tui;
pub mod ui;
pub mod updates;
pub mod util;
pub mod vars;
pub mod web;
pub mod weights;
pub mod wrap;
