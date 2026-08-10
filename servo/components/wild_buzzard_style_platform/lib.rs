/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Linux-only platform contracts required by the imported Stylo engine.

#![forbid(unsafe_code)]
#![deny(missing_docs, warnings)]

#[cfg(not(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_env = "gnu",
    target_vendor = "unknown",
    target_pointer_width = "64",
    target_abi = ""
)))]
compile_error!("Wild Buzzard Stylo supports only x86_64-unknown-linux-gnu");

const _: () = {
    assert!(cfg!(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_env = "gnu",
        target_vendor = "unknown",
        target_pointer_width = "64",
        target_abi = ""
    )));
};

use bitflags::bitflags;
use malloc_size_of::malloc_size_of_is_0;

/// First bit used to encode the Selectors Level 5 heading depth.
pub const HEADING_LEVEL_OFFSET: usize = 57;

bitflags! {
    /// Event-derived state that may participate in selector matching.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct ElementState: u64 {
        /// The element is active.
        const ACTIVE = 1 << 0;
        /// The element has focus.
        const FOCUS = 1 << 1;
        /// The element is hovered.
        const HOVER = 1 << 2;
        /// The form control is enabled.
        const ENABLED = 1 << 3;
        /// The form control is disabled.
        const DISABLED = 1 << 4;
        /// The form control is checked.
        const CHECKED = 1 << 5;
        /// The form control is indeterminate.
        const INDETERMINATE = 1 << 6;
        /// The placeholder is shown.
        const PLACEHOLDER_SHOWN = 1 << 7;
        /// The element is the URL target.
        const URLTARGET = 1 << 8;
        /// The element is fullscreen.
        const FULLSCREEN = 1 << 9;
        /// The form value is valid.
        const VALID = 1 << 10;
        /// The form value is invalid.
        const INVALID = 1 << 11;
        /// The form value is user-valid.
        const USER_VALID = 1 << 12;
        /// The form value is user-invalid.
        const USER_INVALID = 1 << 13;
        /// All validity states.
        const VALIDITY_STATES = Self::VALID.bits() | Self::INVALID.bits() |
            Self::USER_VALID.bits() | Self::USER_INVALID.bits();
        /// The replaced resource is broken.
        const BROKEN = 1 << 14;
        /// The form control is required.
        const REQUIRED = 1 << 15;
        /// The form control is optional.
        const OPTIONAL_ = 1 << 16;
        /// The custom element is defined.
        const DEFINED = 1 << 17;
        /// The link is visited.
        const VISITED = 1 << 18;
        /// The link is unvisited.
        const UNVISITED = 1 << 19;
        /// Either link state.
        const VISITED_OR_UNVISITED = Self::VISITED.bits() | Self::UNVISITED.bits();
        /// The element is a drag-over target.
        const DRAGOVER = 1 << 20;
        /// The form value is in range.
        const INRANGE = 1 << 21;
        /// The form value is out of range.
        const OUTOFRANGE = 1 << 22;
        /// The element is read-only.
        const READONLY = 1 << 23;
        /// The element is read-write.
        const READWRITE = 1 << 24;
        /// The form control is the default.
        const DEFAULT = 1 << 25;
        /// The meter is optimum.
        const OPTIMUM = 1 << 26;
        /// The meter is sub-optimum.
        const SUB_OPTIMUM = 1 << 27;
        /// The meter is sub-sub-optimum.
        const SUB_SUB_OPTIMUM = 1 << 28;
        /// All meter optimum states.
        const METER_OPTIMUM_STATES = Self::OPTIMUM.bits() | Self::SUB_OPTIMUM.bits() |
            Self::SUB_SUB_OPTIMUM.bits();
        /// MathML requests a script-level increment.
        const INCREMENT_SCRIPT_LEVEL = 1 << 29;
        /// Focus should be visibly indicated.
        const FOCUSRING = 1 << 30;
        /// Focus is within the element subtree.
        const FOCUS_WITHIN = 1_u64 << 31;
        /// The resolved direction is left-to-right.
        const LTR = 1_u64 << 32;
        /// The resolved direction is right-to-left.
        const RTL = 1_u64 << 33;
        /// The element has a direction attribute.
        const HAS_DIR_ATTR = 1_u64 << 34;
        /// The direction attribute resolves to left-to-right.
        const HAS_DIR_ATTR_LTR = 1_u64 << 35;
        /// The direction attribute resolves to right-to-left.
        const HAS_DIR_ATTR_RTL = 1_u64 << 36;
        /// The direction attribute behaves as `auto`.
        const HAS_DIR_ATTR_LIKE_AUTO = 1_u64 << 37;
        /// The form control was autofilled.
        const AUTOFILL = 1_u64 << 38;
        /// The form control is showing an autofill preview.
        const AUTOFILL_PREVIEW = 1_u64 << 39;
        /// The element is modal.
        const MODAL = 1_u64 << 40;
        /// The element is inert.
        const INERT = 1_u64 << 41;
        /// The element is the topmost modal.
        const TOPMOST_MODAL = 1_u64 << 42;
        /// Developer tools highlighted the element.
        const DEVTOOLS_HIGHLIGHTED = 1_u64 << 43;
        /// The style editor is transitioning the element.
        const STYLEEDITOR_TRANSITIONING = 1_u64 << 44;
        /// The control value is empty.
        const VALUE_EMPTY = 1_u64 << 45;
        /// The control value is revealed.
        const REVEALED = 1_u64 << 46;
        /// The popover is open.
        const POPOVER_OPEN = 1_u64 << 47;
        /// The slot has assigned nodes.
        const HAS_SLOTTED = 1_u64 << 48;
        /// The openable element is open.
        const OPEN = 1_u64 << 49;
        /// A view transition is active.
        const ACTIVE_VIEW_TRANSITION = 1_u64 << 50;
        /// Print-selection styling is suppressed.
        const SUPPRESS_FOR_PRINT_SELECTION = 1_u64 << 51;
        /// The media element is paused.
        const PAUSED = 1_u64 << 52;
        /// The media element is seeking.
        const SEEKING = 1_u64 << 53;
        /// The media element is buffering.
        const BUFFERING = 1_u64 << 54;
        /// The media element is stalled.
        const STALLED = 1_u64 << 55;
        /// The media element is muted.
        const MUTED = 1_u64 << 56;
        /// Fullscreen was requested with keyboard lock.
        const FULLSCREEN_KEYBOARD_LOCK = 1_u64 << 57;
        /// Packed heading-level bits.
        const HEADING_LEVEL_BITS = 0b1111_u64 << HEADING_LEVEL_OFFSET;
        /// The media element is in picture-in-picture.
        const PICTURE_IN_PICTURE = 1_u64 << 61;
        /// Both resolved direction states.
        const DIR_STATES = Self::LTR.bits() | Self::RTL.bits();
        /// All direction-attribute states.
        const DIR_ATTR_STATES = Self::HAS_DIR_ATTR.bits() | Self::HAS_DIR_ATTR_LTR.bits() |
            Self::HAS_DIR_ATTR_RTL.bits() | Self::HAS_DIR_ATTR_LIKE_AUTO.bits();
        /// Both enabled and disabled states.
        const DISABLED_STATES = Self::DISABLED.bits() | Self::ENABLED.bits();
        /// Both required and optional states.
        const REQUIRED_STATES = Self::REQUIRED.bits() | Self::OPTIONAL_.bits();
    }
}

bitflags! {
    /// Document-global state that may participate in selector matching.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct DocumentState: u64 {
        /// The window is inactive.
        const WINDOW_INACTIVE = 1 << 0;
        /// The application locale is right-to-left.
        const RTL_LOCALE = 1 << 1;
        /// The application locale is left-to-right.
        const LTR_LOCALE = 1 << 2;
        /// Both locale-direction states.
        const ALL_LOCALEDIR_BITS = Self::LTR_LOCALE.bits() | Self::RTL_LOCALE.bits();
    }
}

malloc_size_of_is_0!(ElementState, DocumentState);

/// Returns the configured number of Stylo threads, or `-1` for the local heuristic.
#[must_use]
pub const fn style_thread_count() -> i32 {
    -1
}

/// Registers a style worker with the future local profiler.
///
/// The wave-two platform has no profiler backend, so this deliberately has no side effects.
pub fn register_style_thread(_name: &str) {}

/// Unregisters a style worker from the future local profiler.
///
/// The wave-two platform has no profiler backend, so this deliberately has no side effects.
pub fn unregister_style_thread() {}

/// Reads a compile-time Wild Buzzard style preference.
///
/// Unknown keys fail compilation so imported code cannot silently acquire an arbitrary default.
#[macro_export]
macro_rules! pref {
    // Values mirrored from Firefox ESR153 StaticPrefList.yaml.
    ("browser.display.use_document_fonts") => {
        1_i32
    };
    ("layout.css.outline-offset.snapping") => {
        2_i32
    };
    ("layout.css.stylo-work-unit-size") => {
        16_u32
    };
    ("layout.css.stylo-local-work-queue.in-worker") => {
        0_u32
    };
    ("layout.css.stylo-local-work-queue.in-main-thread") => {
        32_u32
    };

    ("browser.display.permit_backplate") => {
        true
    };
    ("gfx.font_rendering.opentype_svg.enabled") => {
        true
    };
    ("layout.css.anchor-positioning.enabled") => {
        true
    };
    ("layout.css.anchor-positioning.position-try-order.enabled") => {
        true
    };
    ("layout.css.at-scope.enabled") => {
        true
    };
    ("layout.css.basic-shape-shape.enabled") => {
        true
    };
    ("layout.css.color-mix-multi-color.enabled") => {
        true
    };
    ("layout.css.content.alt-text.enabled") => {
        true
    };
    ("layout.css.contrast-color.enabled") => {
        true
    };
    ("layout.css.font-palette.enabled") => {
        true
    };
    ("layout.css.font-tech.enabled") => {
        true
    };
    ("layout.css.gradient-color-interpolation-method.enabled") => {
        true
    };
    ("layout.css.light-dark.images.enabled") => {
        true
    };
    ("layout.css.motion-path-url.enabled") => {
        true
    };
    ("layout.css.properties-and-values.enabled") => {
        true
    };
    ("layout.css.relative-color-syntax.enabled") => {
        true
    };
    ("layout.css.revert-rule.enabled") => {
        true
    };
    ("layout.css.starting-style-at-rules.enabled") => {
        true
    };
    ("layout.css.style-queries.enabled") => {
        true
    };
    ("layout.css.system-ui.enabled") => {
        true
    };
    ("layout.css.webkit-fill-available.enabled") => {
        true
    };
    ("mathml.font_family_math.enabled") => {
        true
    };

    ("dom.select.customizable_select.enabled") => {
        false
    };
    ("dom.viewTransitions.cross-document.enabled") => {
        false
    };
    ("layout.css.appearance-base.enabled") => {
        false
    };
    ("layout.css.attr.enabled") => {
        false
    };
    ("layout.css.background-clip.border-area.enabled") => {
        false
    };
    ("layout.css.control-characters.visible") => {
        false
    };
    ("layout.css.cross-fade.enabled") => {
        false
    };
    ("layout.css.custom-media.enabled") => {
        false
    };
    ("layout.css.ellipse-corners.enabled") => {
        false
    };
    ("layout.css.fit-content-function.enabled") => {
        false
    };
    ("layout.css.grid-template-masonry-value.enabled") => {
        false
    };
    ("layout.css.inverted-colors.enabled") => {
        false
    };
    ("layout.css.margin-rules.enabled") => {
        false
    };
    ("layout.css.overflow-moz-hidden-unscrollable.enabled") => {
        false
    };
    ("layout.css.prefers-reduced-transparency.enabled") => {
        false
    };
    ("layout.css.scroll-driven-animations.enabled") => {
        false
    };
    ("layout.css.scroll-state.enabled") => {
        false
    };
    ("layout.css.stretch-size-keyword.enabled") => {
        false
    };
    ("layout.css.tree-counting-functions.enabled") => {
        false
    };
    ("layout.css.webkit-fill-available.all-size-properties.enabled") => {
        false
    };

    // Wild Buzzard/Servo release policy values absent from Firefox StaticPrefList.yaml.
    ("layout.threads") => {
        -1_i32
    };
    ("layout.columns.enabled") => {
        true
    };
    ("layout.contain.enabled") => {
        true
    };
    ("layout.container-queries.enabled") => {
        true
    };
    ("layout.grid.enabled") => {
        true
    };
    ("layout.variable_fonts.enabled") => {
        true
    };
    ("layout.writing-mode.enabled") => {
        true
    };
    ("layout.css.marker.restricted") => {
        true
    };

    ($unknown:literal) => {
        compile_error!(concat!(
            "unmapped Wild Buzzard style preference: ",
            $unknown
        ))
    };
}

#[cfg(test)]
mod tests {
    use super::{DocumentState, ElementState, HEADING_LEVEL_OFFSET, style_thread_count};

    #[test]
    fn esr_state_bit_assignments_are_stable() {
        assert_eq!(ElementState::ACTIVE.bits(), 1);
        assert_eq!(ElementState::FOCUS_WITHIN.bits(), 1_u64 << 31);
        assert_eq!(
            ElementState::HEADING_LEVEL_BITS.bits(),
            0b1111_u64 << HEADING_LEVEL_OFFSET
        );
        assert_eq!(ElementState::PICTURE_IN_PICTURE.bits(), 1_u64 << 61);
        assert_eq!(DocumentState::LTR_LOCALE.bits(), 1 << 2);
    }

    #[test]
    fn release_preferences_are_typed_and_include_disabled_esr_gate() {
        let threads: i32 = style_thread_count();
        let work_unit: u32 = pref!("layout.css.stylo-work-unit-size");
        let experimental: bool = pref!("layout.css.attr.enabled");
        let contain: bool = pref!("layout.contain.enabled");
        assert_eq!(threads, -1);
        assert_eq!(work_unit, 16);
        assert!(!experimental);
        assert!(contain);
    }
}
