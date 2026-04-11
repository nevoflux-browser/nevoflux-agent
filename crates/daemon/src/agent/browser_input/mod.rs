// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Browser input strategy engine.
//!
//! This module implements the Rust side of the browser input redesign
//! (spec section 6). It exposes two LLM-facing tools:
//!
//! - `browser_input` — high-level tool that probes the target, runs a
//!   pure strategy function to choose an execution plan, invokes the
//!   plan via Actor methods, and verifies the result.
//! - `browser_probe` — escape-hatch tool that returns the raw
//!   `Fingerprint` so LLMs can reason about element context without
//!   running full `browser_input`.
//!
//! Platform adapter recipes (spec section 7) are NOT loaded in PR #2 —
//! the registry is always empty and all decisions fall through to the
//! generic strategy branch.

pub mod bridge;
pub mod error;
pub mod executor;
pub mod fingerprint;
pub mod plan;
pub mod strategy;
pub mod verifier;
