# Wild Buzzard AppImage packaging

Wild Buzzard's sole release format is an AppImage for `x86_64-unknown-linux-gnu`.

This directory will contain reproducible AppDir assembly recipes, desktop metadata, icons owned by
Wild Buzzard, dependency validation, and AppImage launch/relocation tests. Generated AppDirs,
AppImages, debug symbols, logs, and downloaded packaging tools must be written below
`../wildbuzzardbuilds/<agent-or-task>/`, never into this source directory.

The release gate must demonstrate launch from a relocated path on the supported Linux baseline,
Wayland and X11 window creation where available, sandbox startup, local profile creation, normal
website navigation, JavaScript execution, media playback, and no unsolicited network requests.

No Firefox branding, Mozilla service endpoint, embedded updater, telemetry uploader, or
platform-specific payload for Windows, macOS, Android, or iOS belongs in the AppImage.
