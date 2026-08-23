# W9-A6L: live public parity probe and VM launch evidence

Date: 2026-08-23
Owner: main orchestrator
Source base: `bcf2430ba111c459bd1e0c93bae4fa755cb513da`
Initial implementation commit: `de5ceb5275ce6875ab69c558dce4412967c1af23`

## Outcome

This slice turns the public-site comparison checkpoint into a retained generic
test entry point and refreshes the exact Firefox ESR153 YouTube baseline. It
does not claim browser, site, script, interaction, media, screenshot-automation,
or VM parity.

`public_configured_https_reaches_a_desktop_frame` accepts one exact HTTPS URL
through `WILDBUZZARD_PUBLIC_URL`. The implementation contains no hostname,
domain, content, selector, or viewport-threshold branch. It uses the product's
bounded general-web defaults (32 DNS candidates and 16 connection attempts),
an explicit 8 MiB top-level body bound, the system-font comparison policy, and
the existing 1366×768 and 1920×1080 fetch-to-WebRender pipeline. Optional PPM
captures remain outside the source tree. The test prints the non-clear pixel
count even when that count is zero.

The deterministic fixture path deliberately retains its narrower 8-candidate,
8-attempt network limits. Before the public configuration was separated, the
live YouTube lookup failed as
`Network(LimitExceeded { kind: DnsCandidates, limit: 8 })`; using the actual
product defaults then reached both compositor frames. This is a test-policy
correction, not a relaxation of the product network boundary.

## Exact live Firefox reference

The official Mozilla Linux x86-64 `firefox-153.0.1.tar.xz` archive was restored
under the external Data-drive build tree. Its SHA-256 is
`05fb58905a90ce717c36a2ba5af0bbdc4d0e8b0eed6f50469030774c8c85b8eb`,
which exactly matches Mozilla's downloaded `SHA256SUMS` entry. The binary
reports `Mozilla Firefox 153.0.1`.

Standalone geckodriver 0.36.0 opened the exact binary in a visible physical-host
window and exposed WebDriver Classic plus a browser-provided WebDriver BiDi
WebSocket. A fresh isolated external profile disabled about:welcome and product
telemetry submission. The harness navigated to `https://www.youtube.com/`,
waited for document completion, captured the live consent state, clicked the
visible `Reject all` DOM control, waited for the signed-out shell, and captured
it again.

The host's scaled XWayland window resizes in two-pixel increments. With Firefox
forced to device-pixel ratio 1, the closest live CSS viewports are 1366×769 and
1920×1081; this one-row difference is recorded and is not mislabeled as an
exact 1366×768/1920×1080 comparison. Exact-size non-visible comparison runs
remain a separate requirement.

Retained external Firefox hashes:

- consent, 1366×769:
  `c85ab16a0a8de0c5dbf0b23cef494a8a739bb9cb327d2e1376f4b8c01f406f75`;
- rejected-consent shell, 1366×769:
  `53b141e71c92bbdad79033537928ac792f721ed3e655628a5dbe3f8e133f3657`;
- rejected-consent shell, 1920×1081:
  `883bd144ceb4a8632d4aa446b224f835fa5b8ecefa1b1e1dde7927978057c4b0`.

The same live session then navigated to the public
`https://www.youtube.com/watch?v=jNQXAC9IVRw` reference. Firefox published the
title `Me at the zoo - YouTube`; its media element reported ready state 4, a
320×240 decoded stream, and a 19.013042-second duration. A real WebDriver
element click changed the player from paused to playing, `currentTime` advanced
by 4.009229 seconds during the observation interval, and a second element click
returned the player to paused state. The retained JSON trace has SHA-256
`21ce968d0623744fd63bcb1305d3d46d328bee7e8e52b335429d1b19b846dc43`.
The before-play, playing, and paused screenshots respectively hash to
`b48065700f5ece495faf924b3cc404ecfb131d6d8fbd2575d82009aff9fc4dcc`,
`2bebf353703068b1f7f0ac364eec9c83fc6e95a0e6deab64504455bd1fcd3c77`, and
`41df8fecd06e2c56f83ca706762429569bd4cf856035dd06f2302506043ce0be`.
This is reference evidence for the later identical Wild Buzzard sequence, not
Wild Buzzard media evidence.

A second retained control trace exercises the same paused player through real
WebDriver input: the `J` key seeks from 14.928875 to 4.928875 seconds exactly;
the mute control transitions false → true → false; the captions control
transitions false → true → false; and the fullscreen control plus Escape
transitions false → true → false. Its JSON SHA-256 is
`e915e35931e6160248aed927651e129604980aa82516e5bfc86ee6fd63ffe2f3`.
The muted, captions, and fullscreen screenshot hashes are respectively
`2575b919e503d3db9335d79e3b434e8f1e99ae71301cb4d9ab7e14cd3606dbe7`,
`c862d2bdea5643bddf554f527ad9f24fb02a86d91343d332ba8e4c23fa593e03`, and
`5623f6b359fc72f5703b7d819d9b54baff6f7df1403cc1297ae49dc05857c073`.

## Current Wild Buzzard YouTube result

The same exact URL completed the generic top-level pipeline at both retained
headless comparison viewports. Both valid RGBA8 frames contained exactly zero
non-clear pixels:

- 1366×768 PPM:
  `c4ed69476ac999a8883eb1cf394d823f3473797eeb0ae074850a9de0da2d2875`;
- 1920×1080 PPM:
  `12f63567b2d7380f5e8ad813f88cb21444191514bee77151c5a9f36ad79738ed`.

This is the expected observable gap while external resource admission, page
JavaScript/modules/promises/jobs, DOM host bindings, dynamic invalidation,
images, and media remain disconnected. A blank successful frame is not a
YouTube or normal-site result.

### Machine-readable pixel difference

The signed-out Firefox shell was compared to the blank Wild Buzzard frame as
raw sRGB bytes at each exact Wild Buzzard viewport. The only alignment step
excluded Firefox's unavoidable extra bottom row; no scale, color correction,
threshold, or perceptual equivalence rule was applied. At 1366×768,
1,048,735 of 1,049,088 pixels differ (99.9663517%), with mean absolute channel
error 234.025257. At 1920×1080, 2,073,247 of 2,073,600 pixels differ
(99.9829765%), with mean absolute channel error 236.660907. The retained JSON
report hashes to
`4f1869ca7d093a3064dccbc7044ff4bdb4ec529046afdaba2902c71055faeb9d`;
the ×3 desktop and full-HD difference heatmaps hash to
`43a8ae89dd0bb20eb62de84fdf0ea51af72a56df960ac4c657c920f492c66654` and
`5d89be99038a154eab0c8b6e97b872327c8aea993a1dee7333b8fd27e9a18b62`.
These deliberately poor numbers are the initial regression baseline, not a
quality score or parity threshold.

## Physical-host and VM execution

The fresh release shell was built in the Data-rooted Podman environment in
36.31 seconds. The output is a 27,502,104-byte Linux x86-64 ELF with SHA-256
`0a702b356ba9295a6911754883e749a9509a8f750bc6e05530c6006861e44e6d`.
It opens a live native Wayland window on the physical host. A forced X11 launch
currently fails earlier because the host lacks dynamically opened
`libxkbcommon-x11.so`; that packaging dependency remains explicit.

All three existing libvirt test VMs are controllable through the user-session
hypervisor and QEMU guest agent. Ubuntu 24.04 and Ubuntu 26.04 expose ordinary
Virtio GPU `card0` and `renderD128` nodes. The release binary was transferred to
Ubuntu 24.04, its in-guest SHA-256 matched exactly, and it was launched in the
active GNOME session with the session's exact Wayland, X11, D-Bus, runtime, and
Xauthority environment. It failed safely and reproducibly at the open product
gate:

```text
InitializeRenderer/Renderer: WebRender initialization failed: SoftwareRasterizer
```

No frame was submitted and partial ownership reported checked teardown. This
proves transfer and live-VM control; it also proves that the current
hardware-only renderer is not the required CPU fallback.

The Debian 13 VM was subsequently restored from its experimental Venus/blob and
EGL-headless definition to the same ordinary virtio display class used by the
Ubuntu software-rendering controls. The exact pre-change XML was retained with
SHA-256 `0700a4f697a3c163f0c005913a0c5976629fc66c24e385351665f8ab00d7cbd1`;
the validated standard definition hashes to
`a9f053657eaea1e748723868ab941a0ea41f17acf20a9130ba75b8b26426da1b`.
The wedged guest ignored guest-agent and ACPI shutdown, so only this named test
VM was force-powered off after the backup. It booted cleanly with active GDM,
an active Debian Wayland session, ordinary virtio `card0` and `renderD128`, and
a visible 1280×800 VNC desktop. The retained scanout PNG hashes to
`2221238ab0cf35d212d5a8d6eb642c34c568b1ec7b6d43623f4ec73aadc82048`.

The exact 27,502,104-byte release was then transferred into this restored VM;
its SHA-256 again matched. Launching through the real Debian user session fails
safely at the same explicit
`InitializeRenderer/Renderer: WebRender initialization failed:
SoftwareRasterizer` gate with zero submitted frames and checked partial-owner
teardown. The standard Debian CPU-rendering environment is now available for
the incoming fallback implementation; passing it remains open.

Ubuntu 26.04 was assigned as the ordinary accelerated-VM row. Its original
virtio definition hashes to
`453d3ff99d7c5bb3318768b59ba4cd142741d9a3a905dff45a955dab1482c8f9`.
The final validated local SPICE-GL definition uses only `virtio-vga-gl`,
`accel3d=yes`, and `/dev/dri/renderD128`; it has no Venus/blob property,
passthrough, or driver-forcing environment and hashes to
`858f8df55ecfccb18ff0dcf78c2fb271d012d84b0d09168fb52faf1147658984`.
The guest boots an active Ubuntu Wayland session and its kernel reports
`[drm] features: +virgl +edid -resource_blob -host_visible`.

The exact matching release copy reaches the distinct accelerated-VM blocker
`CreateContext/Driver: context robustness is not supported`. This validates
the need for the incoming typed compatible-context profile; it is not a
passing accelerated frame. VNC plus EGL-headless produced no QEMU `screendump`
surface, so the VM was moved to local SPICE GL for Virt-Manager. Legacy
`virsh screenshot` still cannot capture that GL scanout. Qualifying visual
evidence therefore remains bound to the new browser-compositor readback or an
equivalent authenticated Virt-Manager/SPICE capture, never a fabricated
headless frame.

## Verification

- the complete engine workspace passes 39 library and 71 integration tests
  (110 total); both public-network tests remain explicitly ignored by default;
- the first broad rerun exposed one stale test-only assumption that exact
  glyph coverage would include coordinate `(19,10)`. The worker's real frame
  had the declared panel background there while retaining correctly blended
  text pixels. The assertion now searches the bounded panel interior for a
  pixel between the declared background and foreground colors. The focused
  test and complete matrix pass; no production renderer or pixel changed;
- the generic YouTube probe passes at both configured viewports and reports
  zero non-clear pixels;
- the retained example.com public test still passes at both viewports after
  the public network-policy separation;
- strict all-target owner-workspace Clippy with all and pedantic warnings denied
  passes when the reusable Data target is mounted at its original
  `/build/cargo` path (an initial alternate mount exposed stale absolute shader
  include paths, not a source diagnostic);
- the browser shell release build passes in the Data-drive container;
- Firefox and Wild Buzzard artifacts, Cargo targets, profiles, logs, VM
  transfer files, and screenshots remain under
  `/run/media/user/Data/Repositories/wildbuzzardbuilds`;
- `docs/parity/site-compatibility.toml` records the exact artifacts and the
  unresolved gaps without a parity claim.

The final generic-navigation source hash is
`59d2f1690b4dd12252e61869a76b22518b73aa3982fcd6e8f1b17af9d9da4172`;
the hardened static-pipeline test hash is
`9edad64a2e376de3c68b2b9b9e96f9872be5feb81e53dbda4223495f997563bd`.

## Next gates

1. Integrate browser-internal automation with the real native compositor and
   exact frame identity so visible Wild Buzzard actions and screenshots can be
   paired with the Firefox BiDi trace.
2. Add the reviewed CPU presentation backend and standard virgl capability
   path, then rerun the physical/Ubuntu/Debian matrix.
3. Connect CSP-governed external stylesheet/resource admission and the
   continuously staffed Brimstone script/runtime lane to the document event
   loop, then rerun this exact YouTube probe and record the next generic gap.
