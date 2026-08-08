/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Rust-native atom interning for the Wild Buzzard Stylo platform.

#![forbid(unsafe_code)]
#![deny(missing_docs, warnings)]

// string_cache_codegen emits machine-generated PHF tables and packed numeric literals.
#[allow(clippy::pedantic, dead_code, missing_docs)]
#[doc(hidden)]
pub mod generated {
    include!(concat!(env!("OUT_DIR"), "/atom.rs"));
}

pub use generated::{Atom, AtomStaticSet};

/// Interns a string in the Wild Buzzard CSS atom domain.
#[macro_export]
macro_rules! atom {
    ($value:tt) => {
        $crate::wild_buzzard_static_atom!($value)
    };
}

#[cfg(test)]
mod tests {
    use super::Atom;

    #[test]
    fn equal_text_has_equal_atoms() {
        let first = Atom::from("display");
        let second = Atom::from(String::from("display"));
        assert_eq!(first, second);
    }
}
