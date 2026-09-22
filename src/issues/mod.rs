//! Issue classification, duplicate marking, and independently verified draft fixes.
pub mod config;
mod github;
mod triage;
mod worker;
mod workflow;

pub use worker::run_child;
pub use workflow::run;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Item {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub open: bool,
    pub is_pr: bool,
    pub updated_at: String,
    pub comments: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    Duplicate,
    Addressed,
    NeedsInfo,
    NeedsDecision,
    Ready,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub route: Route,
    pub labels: Vec<String>,
    pub matches: Vec<u64>,
    pub related: Vec<u64>,
    /// Internal evidence gaps; never render these as related work.
    #[serde(default)]
    pub unresolved: Vec<u64>,
    /// Concrete worker guidance suitable for publication, when available.
    #[serde(default)]
    pub guidance: Option<String>,
    pub explanation: String,
}
