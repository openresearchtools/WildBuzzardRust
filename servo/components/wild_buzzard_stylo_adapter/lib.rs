/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Safe immutable-DOM embedding for the imported Stylo CSS engine.
//!
//! This crate does not implement CSS parsing, selector matching, cascading,
//! or computed values. It adapts a [`wild_buzzard_dom::DocumentSnapshot`] to
//! Stylo's generic DOM traits and publishes Stylo's results through the
//! revisioned [`wild_buzzard_layout::ComputedStyleSnapshot`] contract.

#![deny(missing_docs, warnings)]

mod embedding;
mod engine;
mod error;
mod state;
mod translate;

pub use engine::{
    prepare_computed_styles, prepare_computed_styles_with_states, ComputedStyloSnapshot,
    StaticStyleOptions, StyleDiagnostic, StyleLimits,
};
pub use error::{StyleAdapterError, UnsupportedComputedValue};
pub use state::{
    ElementSelectorState, SelectorState, SelectorStateSnapshot, SelectorStateSnapshotError,
};
