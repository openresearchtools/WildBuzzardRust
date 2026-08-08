/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Compatibility facade exposing Wild Buzzard's typed Stylo preferences.

#![forbid(unsafe_code)]
#![deny(missing_docs, warnings)]

pub use wild_buzzard_style_platform::pref;

#[cfg(test)]
mod tests {
    #[test]
    fn reexport_preserves_preference_types() {
        let enabled: bool = crate::pref!("layout.grid.enabled");
        let queue_size: u32 = crate::pref!("layout.css.stylo-local-work-queue.in-main-thread");
        assert!(enabled);
        assert_eq!(queue_size, 32);
    }
}
