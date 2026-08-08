/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::time::Duration;

pub struct Telemetry;

pub struct TimerId;

/// No-op timing interface retained at the renderer boundary.
impl Telemetry {
    pub fn record_rasterize_blobs_time(_duration: Duration) { }
    pub fn start_framebuild_time() -> TimerId { TimerId }
    pub fn stop_and_accumulate_framebuild_time(_id: TimerId) { }
    pub fn record_renderer_time(_duration: Duration) { }
    pub fn record_renderer_time_no_sc(_duration: Duration) { }
    pub fn record_scenebuild_time(_duration: Duration) { }
    pub fn start_sceneswap_time() -> TimerId { TimerId }
    pub fn stop_and_accumulate_sceneswap_time(_id: TimerId) { }
    pub fn cancel_sceneswap_time(_id: TimerId) { }
    pub fn start_shaderload_time() -> TimerId { TimerId }
    pub fn stop_and_accumulate_shaderload_time(_id: TimerId) { }
    pub fn record_texture_cache_update_time(_duration: Duration) { }
    pub fn record_time_to_frame_build(_duration: Duration) { }
}
