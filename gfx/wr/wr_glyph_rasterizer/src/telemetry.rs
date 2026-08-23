/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

pub struct TimerId;

pub struct Telemetry;

/// No-op timing interface retained at the glyph-rasterizer boundary.
impl Telemetry {
    // Start rasterize glyph time collection
    pub fn start_rasterize_glyphs_time() -> TimerId {
        return TimerId {};
    }
    // End rasterize glyph time collection
    pub fn stop_and_accumulate_rasterize_glyphs_time(_id: TimerId) {}
}
