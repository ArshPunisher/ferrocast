# Studio parity: everything OBS does, and where Ferrocast does better

Owner, 25 Oct 2026: the Studio (2.1, Track B in LIVE-STUDIO.md) should have every OBS feature,
"even very tiny minor features", and be better on our side. This is the full map, taken from
OBS's own source (32.0.2, `obsproject/obs-studio` plus obs-browser), so nothing is missed by
memory. Appendix A lists every source, filter and transition with every option; Appendix B
every setting, menu and dialog of the OBS app.

## How parity is reached

The Studio embeds **libobs and OBS's own plugins** (LIVE-STUDIO.md, Track B). That decides most
of it:

| Layer | What it holds | How Ferrocast gets it |
|---|---|---|
| Engine (libobs) | Scenes, groups, nested scenes, items and transforms, audio mixing, 6 tracks, monitoring, sync offset, color formats, HDR, rendering, encoders, outputs | Same code: identical behaviour |
| Plugins | 34 sources, 27 filters, 7 transitions, 41 encoders, 14 outputs (Appendix A), their 876 options | Same plugins loaded by the Studio: every source and filter works the same, including third-party OBS plugins (marked, LIVE-STUDIO Plugins) |
| Options UI | Each source's and filter's settings | **One generic renderer for `obs_properties`** (bool, int, float, text, path, list, color, font, button, editable list, frame rate, group): every option of every plugin appears with no per-plugin work, so tiny options can't be forgotten |
| App (frontend) | Settings pages, menus, docks, studio mode, multiview, projectors, hotkeys, profiles, scene collections, importers, stats, undo (Appendix B) | Built by us, item by item, against Appendix B. This is the real work |

### Browser source: needs a decision (25 Oct)

obs-browser loads (Qt and obs-frontend-api are preloaded from OBS.app) but can't run inside
Ferrocast yet, for two reasons found on 25 Oct:

1. It looks for Chromium (CEF) and its four helper apps in **Ferrocast's own** `Contents/Frameworks`
   (next to the running program), not in OBS.app. The shipped app would have to carry them
   (about 250 MB, the size OBS.app spends on them).
2. With CEF linked beside a dev build it aborts at start (SIGTRAP): Chromium on macOS needs the
   app's `NSApplication` to implement its `CrAppProtocol` / `CrAppControlProtocol`, which OBS's
   Qt app does and Tauri's doesn't; CEF's message loop also has to be pumped on the main thread.

Two ways forward:

| | A. Bundle CEF like OBS | B. Browser source on the system WebKit |
|---|---|---|
| What | Ship CEF + helpers in Ferrocast.app, add the CrApp methods to Tauri's NSApplication class at start, pump CEF's loop | A Ferrocast source (libobs `obs_source_info`) that draws a WKWebView offscreen and hands frames to libobs |
| Same as OBS | Yes, page for page (same Chromium) | Nearly: WebKit, not Chromium (a few CSS or codec differences; OBS's `window.obsstudio` API to be provided by us) |
| Size | +~250 MB download | +0 |
| Risk | CEF inside a non-CEF app on macOS: crash-prone, every CEF update | Our own code to keep up, but small and in our hands |

Ferrocast's own overlays work either way. Recommended: **B first** (small, ours), keep A for
pages that only work in Chromium. Until then the Studio shows sources without the Browser kind.

## Checklist by area

Phase: **P1** = 2.1 preview, **P2** = daily driver (2.2), **P3** = later. ✅ already in Ferrocast
(Track A or earlier). Each line ends with how Ferrocast does it better, when it can.

### Scenes and sources
- [x] P1 Scenes: add, rename, duplicate, delete, reorder, scene filters (26 Oct; grid mode too). *Better: start from
  ready screens (themes, 8 screens) instead of an empty scene.*
- [x] P1 Sources: add (new or existing), rename, duplicate (reference or copy), delete, reorder,
  show/hide, lock, groups and nested scenes, "Paste (Reference/Duplicate)", copy/paste filters.
  (26 Oct: groups made, filled, emptied and taken apart; Paste and Paste a copy.)
- [x] P1 Transform: position, rotation, size, crop, bounds (8 kinds), alignment (9 points),
  flip, fit/stretch/center to screen, reset, copy/paste transform, edit transform dialog,
  snapping (screen, sources, center) with sensitivity.
- [x] P2 Per item: blending mode (8) and method, scale filtering (6), deinterlacing (7 modes,
  field order), show/hide transitions with duration, "Resize output (canvas) to source size" (26 Oct, blending method too).
- [x] P1 Properties, filters, interact (browser and windows), screenshot of a source, open
  source projector, "Hide in mixer".
- [x] P2 Missing files dialog (find, replace, search folder). *Better: found automatically by
  name in the folders you choose.* (26 Oct: a notice when files are missing; Search a folder
  fixes every one found by name at once.)
- [ ] P1 Every source in Appendix A for Mac and Windows (macOS Screen Capture, Display/Window
  Capture, Game Capture, Video Capture Device, Audio Input/Output Capture, Application Audio,
  Media, Image, Slide Show, Color, Text, Browser, VLC, Blackmagic, Syphon, NDI via plugin).
  *Better: devices are found and named for you ("Your camera: Logitech C920", "Your mic:
  Maonocaster"), and one that unplugs comes back by itself with a plain notice.*

### Audio
- [ ] P1 Mixer: volume (dB/%), mute, meters (peak, input peak, RMS), push-to-talk and push-to-mute
  with delays, monitoring (off / monitor only / monitor and output), monitoring device.
- [x] P2 Advanced audio: balance, mono, sync offset, tracks 1 to 6 per source (26 Oct evening: one table for every sound, cards on a phone).
- [x] P1 Audio settings: sample rate, channels (mono to 7.1), global devices (2 desktop, 4 mic),
  meter decay and peak meter type, low-latency buffering. (26 Oct; meter decay and true peak done too; the mixer can show only sounds making sound. Low-latency buffering (Windows) not yet.)
- [ ] P1 Filters: noise suppression (RNNoise, Speex, NVIDIA), noise gate, gain, compressor,
  upward compressor, expander, limiter, 3-band EQ, invert polarity, VST 2.
  *Better: "Make my mic sound good" adds the right chain from a 5-second check, and warns while
  live if the mic is muted, too quiet, clipping or picking up the game twice.*
- [ ] P2 *Better: a Twitch VOD track (music left out of the VOD) set up in one switch.*

### Video and rendering
- [ ] P1 Base (canvas) and output resolution, downscale filter (bilinear, area, bicubic,
  Lanczos), common FPS and fractional/integer FPS.
- [ ] P2 Color format (NV12, I420, I444, P010, I010, P216, P416, BGRA), color space (sRGB,
  709, 601, 2100 PQ/HLG), range, SDR white and HDR nominal peak levels.
- [ ] P1 Filters: color correction, LUT, chroma/color/luma key, crop/pad, scaling/aspect,
  scroll, sharpen, image mask/blend, render delay, async video delay, HDR tone mapping,
  NVIDIA background removal/blur. *Better: one switch "Remove background" on the webcam using
  whatever the computer has (NVIDIA, or a built-in model on Mac).*

### Transitions
- [x] P1 Cut, fade, swipe, slide, fade to color, luma wipe, stinger (video, transition point,
  audio fade style, track matte), duration, per-scene override, quick transitions (26 Oct).
- [x] P2 Studio mode: preview/program, transition button, quick transitions, T-bar (held, back,
  finish as OBS), swap, duplicate scene and sources, swap after transition, transition on
  double-click, fade to black (26 Oct). OBS's own bugs avoided: an aborted T-bar leaving the
  wrong scene on air, a click mid-transition, the T-bar track acting on clicks. *Better: our motion themes ship matching stingers.*

### Streaming
- ✅ Multistream to Twitch, YouTube, Kick and custom servers, health, reconnects, upload check,
  upload protection, platform rules, per-platform info, vertical (Track A).
- [x] P1 Stream settings: services list, custom server (RTMP, RTMPS, SRT, RIST, WHIP/WebRTC),
  auth, bandwidth test mode, "ignore streaming service setting recommendations". (26 Oct: SRT,
  RIST and WebRTC go straight from the Studio, alongside RTMP destinations, each with health.
  Services and bandwidth test: Go live's platforms and upload check. Bind IP, TCP pacing and
  the RTMP network options only touch the Studio's hop to this PC, so not copied.)
- [ ] P2 Twitch Enhanced Broadcasting (multitrack video), YouTube HDR/HEVC/AV1. (26 Oct:
  parked for the owner. OBS asks Twitch's GetClientConfiguration with the stream key, runs
  several encoders and sends them in one enhanced-RTMP stream straight to Twitch; the Studio
  streams through Go live's relay, so it needs its own direct Twitch output, and it can only
  be checked on a real Twitch channel going live. HEVC/AV1 encoders are already pickable.)
- [x] P1 Output: simple and advanced modes, encoder per output, rate control (CBR, VBR, CQP,
  CRF, ABR), bitrate, keyframe interval, preset/tuning/profile, B-frames, lookahead,
  psycho-visual tuning, GPU selection, audio encoder and bitrate per track, rescale output.
  (26 Oct: plain controls first, then "Every option of this encoder" from the encoder itself;
  any encoder for the stream and the recording.)
  *Better: chosen by a speed and GPU test with the reason in words; "encoder overloaded" turns
  into a plain fix or an automatic step down.*
- [ ] P1 Advanced: stream delay (and preserve cutoff point), automatic reconnect (retry delay,
  max retries), bind to IP, network optimisations, TCP pacing, low-latency mode.

### Recording, replay, virtual camera
- [x] P1 Recording: path, format (MKV, MP4, MOV, hybrid/fragmented MP4/MOV, FLV, TS, HLS),
  quality presets, encoder, tracks, custom muxer settings, split file (by time/size, manual),
  filename formatting with variables, overwrite, generate without spaces, auto-remux to MP4,
  pause recording, chapter markers. (26 Oct, HLS and custom muxer settings included.)
  *Better: a backup recording while live, and chapter markers from stream events (raids,
  big tips).*
- ✅ Replay buffer and "Clip that" (Studio, today through OBS). In the Studio itself too (26 Oct: seconds, size, format, tracks, Save replay, shortcuts).
- [~] P2 Virtual camera (scene, source or program output): built through OBS's own (26 Oct). Windows: OBS's registered camera. macOS: OBS's camera extension can only be turned on by OBS.app, so it refuses from Ferrocast; our own needs an Apple Developer account (NEEDS-FROM-YOU 4). Remux dialog (done 26 Oct).

### App
- [x] P1 Profiles and scene collections: new, duplicate, rename, remove, import, export.
  Import from OBS (26 Oct) and from Streamlabs Desktop (26 Oct, its format turned into OBS's as
  OBS's importer does; Windows devices come in to be picked again). XSplit and Twitch Studio
  (discontinued) not planned.
- [ ] P1 Settings: General (language, theme, tray, confirmations, snapping, projectors, system
  tray), Hotkeys (every action, search, duplicates warning), Accessibility (colors, meters),
  Appearance (themes, font size, density). (26 Oct: Hotkeys done: every OBS action incl.
  show/hide, filters, media, quick transitions, push to mute, with search; a key moved from
  another action says so. Media controls in the Sources menu too. General done: snapping
  on/off, distance, to screen edges, other sources, the middle (Alt skips it); asking first
  before going live, ending the stream (default) or stopping a recording; recording and the
  replay buffer starting with the stream and stopping with it unless kept. Language, theme
  and tray are the dashboard's own.)
- [x] Projectors (window or full screen per display, Esc to leave) and a multiview (click to
  switch), 26 Oct.
- [x] P2 Docks (lock, reset, custom browser docks), Stats dock (CPU, memory, disk, FPS, render
  lag, encoding lag, dropped frames per output), multiview (layouts, labels), projectors
  (fullscreen per monitor, windowed, always on top). (26 Oct: Stats done, opened from the
  stats bar, with how long the disk lasts at the recording's bitrate; docks lock and reset
  done earlier; multiview layouts (above, below, left, right, scenes only), Studio Mode's
  preview in it, names and safe areas, and projectors always on top done too. Custom browser
  docks done 26 Oct as web panels: any page as a panel, moved, popped out and hidden like the
  rest; sites that refuse to be framed open in a window of their own in the app. *Better:*
  Ferrocast's merged chat (Twitch, YouTube, Kick) and event feed are panels too.)
- [x] P1 Undo/redo (26 Oct: sources, scenes, filters, settings, sound, item transitions, deinterlacing; shortcuts aren't steps). *Better: a history list you can step back through (done).*
- [x] P2 Auto-configuration wizard, log files (upload, view, crash reports), check for updates,
  plugin manager, safe mode, "show recording/streaming in title bar". (26 Oct: Set up for me,
  sharing the upload between platforms and keeping each platform's limit; the log in the app
  with problems coloured and keys taken out, copied rather than uploaded; low latency audio
  buffering. Check for updates is the app's own updater; crash reports and panics go to the
  same log. Plugin manager and safe mode don't apply: the Studio loads OBS's own plugins.)
- [ ] P1 *Better: settings search; plain-language errors with the fix (R-08); a phone as a
  remote and a preview (Dashboard anywhere, already there).*

## Not copied on purpose

- TikTok anywhere (R-26).
- Old Windows-only ways kept by OBS for compatibility where a modern one does the same
  (decided per item when building, and listed here).

## Appendix A: every source, filter and transition, with its options

Generated from OBS 32 source (`plugins/*`, en-US labels). Platform-only items are marked by plugin (mac-, win-, linux-).

### Sources

- **Blackmagic Device** (`decklink-input`, decklink): Device; Video Connection; Audio Connection; Mode; Pixel Format; Color Space; Color Range; Channel; Swap FC and LFE; Use Buffering; Deactivate when not showing; Allow 10 Bit (Required for SDI captions, may cause performance overhea
- **Color Source** (`color_source`, image-source): color; width; height
- **Image** (`image_source`, image-source): file; unload; linear_alpha
- **Image Slide Show** (`slideshow`, image-source): PlaybackBehavior; SlideMode; Transition; SlideTime; TransitionSpeed; PlaybackMode; HideWhenDone; CustomSize; Files
- **Audio Capture Device (ALSA)** (`alsa_input_capture`, linux-alsa): device_id; custom_pcm; rate
- **Display Capture (XSHM)** (`xshm_input`, linux-capture): screen; show_cursor; advanced; cut_top; cut_left; cut_right; cut_bot; server
- **Display Capture (XSHM)** (`xshm_input_v2`, linux-capture): screen; show_cursor; advanced; cut_top; cut_left; cut_right; cut_bot; server
- **JACK Input Client** (`jack_output_capture`, linux-jack): channels; startjack
- **Screen Capture (PipeWire)** (`pipewire-desktop-capture-source`, linux-pipewire): Reload; ShowCursor
- **Video Capture Device (PipeWire) (BETA)** (`pipewire-camera-source`, linux-pipewire): device_id; format; framerate; controls
- **Audio Input Capture (PulseAudio)** (`pulse_input_capture`, linux-pulseaudio): device_id
- **Audio Output Capture (PulseAudio)** (`pulse_output_capture`, linux-pulseaudio): device_id
- **Video Capture Device (V4L2)** (`v4l2_input`, linux-v4l2): device_id; input; pixelformat; standard; dv_timing; resolution; framerate; color_range; buffering; auto_reset; timeout_frames; controls
- **Video Capture Device** (`macos-avcapture`, mac-avcapture): Device; Use Preset; buffering; enable_audio_button; Preset; Resolution; Frame rate; Input format; Color space; Video range
- **Video Capture Device (macOS)** (`macos-avcapture`, mac-avcapture): Device; Use preset; Preset; Resolution; Frame rate; Video format; Color space; Video range; Use buffering
- **Audio Input Capture** (`coreaudio_input_capture`, mac-capture): Device; enable_downmix
- **Audio Output Capture** (`coreaudio_output_capture`, mac-capture): Device; enable_downmix
- **Display Capture** (`display_capture`, mac-capture): display_uuid; show_cursor; crop_mode; name; window; show_empty_names
- **Window Capture** (`window_capture`, mac-capture): show_shadow; window; show_empty_names
- **macOS Audio Capture** (`sck_audio_capture`, mac-capture): type; application; reactivate_capture
- **macOS Screen Capture** (`screen_capture`, mac-capture): type; display_uuid; application; window; show_empty_names; show_hidden_windows; show_cursor; hide_obs; reactivate_capture; audio_info; capture_type_info
- **Syphon Client** (`syphon-input`, mac-syphon): uuid; allow_transparency; crop; syphon license
- **Browser** (`browser_source`, obs-browser): Local file; URL; Width; Height; Control audio via OBS; Use custom frame rate; FPS; Custom CSS; Shutdown source when not visible; Refresh browser when scene becomes active; Refresh cache of current page; Page permissions (none, read OBS status, read user info, basic, advanced, full)
- **Media Source** (`ffmpeg_source`, obs-ffmpeg): is_local_file; local_file; looping; restart_on_activate; buffering_mb; input; input_format; reconnect_delay_sec; hw_decode; clear_on_media_end; close_when_inactive; speed_percent; color_range; linear_alpha; seekable; ffmpeg_options
- **Text (GDI+)** (`text_gdiplus`, obs-text): Font; Read from file; Text; Text File (UTF-8); Enable Antialiasing; Text Transform; Vertical; Color; Opacity; Gradient; Gradient Color; Gradient Opacity; Gradient Direction; Background Color; Background Opacity; Alignment; Vertical Alignment; Outline; Outline Size; Outline Color; Outline Opacity; Chatlog Mode; Chatlog Line Limit; Use Custom Text Extents; Width; Height; Wrap
- **Audio Input Capture (OSS)** (`oss_input_capture`, oss-audio): OBS_PROPS_DSP; OBS_PROPS_CUSTOM_DSP; OBS_PROPS_RATE; OBS_PROPS_CHANNELS; OBS_PROPS_SAMPLE_FMT
- **Sndio input client** (`sndio_output_capture`, sndio): device; rate; bits; channels
- **Text (FreeType 2)** (`text_ft2_source`, text-freetype2): font; from_file; text_file; text; antialiasing; log_mode; log_lines; color1; color2; outline; drop_shadow; custom_width; word_wrap
- **VLC Video Source** (`vlc_source`, vlc-video): Loop Playlist; Shuffle Playlist; Visibility behavior; Playlist; Network Caching; Audio Track; Subtitles Enabled; Subtitle Track
- **Display Capture** (`monitor_capture`, win-capture): Display; Multi-adapter Compatibility; Capture Cursor
- **Game Capture** (`game_capture`, win-capture): Mode; Window; Window Match Priority; SETTINGS_COMPAT_INFO; Capture Audio (BETA); SLI/Crossfire Capture Mode (Slow); Allow Transparency; Premultiplied Alpha; Limit capture framerate; Capture Cursor; Use anti-cheat compatibility hook; Capture third-party overlays (such as steam); Hook Rate; RGB10A2 Color Space
- **dshow_input** (`dshow_input`, win-dshow): Device; activateText; Configure Video; Configure Crossbar; Deactivate when not showing; Resolution/FPS Type; Resolution; FPS; Video Format; Color Space; Color Range; Buffering; Flip Vertically; Apply rotation data from camera (if any); Use hardware decoding when available; Audio Output Mode; Use custom audio device; Audio Device
- **Application Audio Capture (BETA)** (`wasapi_process_output_capture`, win-wasapi): OPT_WINDOW; OPT_PRIORITY
- **Audio Input Capture** (`wasapi_input_capture`, win-wasapi): OPT_DEVICE_ID; OPT_USE_DEVICE_TIMING
- **Audio Output Capture** (`wasapi_output_capture`, win-wasapi): OPT_DEVICE_ID; OPT_USE_DEVICE_TIMING

### Filters

- **NVIDIA Audio Effects** (`nvidia_audiofx_filter`, nv-filters): Method; Suppression Intensity; Voice Optimized; deprecation
- **NVIDIA Background Blur Filter** (`nv_background_blur_filter`, nv-filters): Blur Intensity; Mode; Threshold; Mask refresh frequency in frames; deprecation
- **NVIDIA Background Removal** (`nv_greenscreen_filter`, nv-filters): Blur Intensity; Mode; Threshold; Mask refresh frequency in frames; deprecation
- **NVIDIA Blur Filter** (`nv_blur_filter`, nv-filters): Blur Intensity; Mode; Threshold; Mask refresh frequency in frames; deprecation
- **3-Band Equalizer** (`basic_eq_filter`, obs-filters): name
- **Apply LUT** (`clut_filter`, obs-filters): Info: Settings only apply to SDR video.; Path; Amount; Passthrough Alpha
- **Chroma Key** (`chroma_key_filter`, obs-filters): Key Color Type; Key Color; Similarity (1-1000); Smoothness (1-1000); Key Color Spill Reduction (1-1000); Opacity; Contrast; Brightness; Gamma
- **Color Correction** (`color_filter`, obs-filters): Gamma; Contrast; Brightness; Saturation; Hue Shift; Opacity; Color
- **Color Key** (`color_key_filter`, obs-filters): Key Color Type; Key Color; Similarity (1-1000); Smoothness (1-1000); Opacity; Contrast; Brightness; Gamma
- **Compressor** (`compressor_filter`, obs-filters): Ratio; Threshold; Attack; Release; Output Gain; Sidechain/Ducking Source
- **Crop/Pad** (`crop_filter`, obs-filters): relative; left; top; right; bottom; cx; cy
- **Expander** (`expander_filter`, obs-filters): Presets; Ratio; Threshold; Attack; Release; Output Gain; Detection; Knee Width
- **Gain** (`gain_filter`, obs-filters): Gain
- **HDR Tone Mapping (Override)** (`hdr_tonemap_filter`, obs-filters): override_info; transform; sdr_white_level_nits; hdr_input_maximum_nits; hdr_output_maximum_nits; sdr_input_maximum_nits; sdr_output_maximum_nits
- **Image Mask/Blend** (`mask_filter`, obs-filters): Info: Settings only apply to SDR video.; Type; Path; Color; Opacity; Stretch Image (discard image aspect ratio)
- **Invert Polarity** (`invert_polarity_filter`, obs-filters): no options
- **Limiter** (`limiter_filter`, obs-filters): Threshold; Release
- **Luma Key** (`luma_key_filter`, obs-filters): Info: Settings only apply to SDR video.; Luma Max; Luma Max Smooth; Luma Min; Luma Min Smooth
- **Noise Gate** (`noise_gate_filter`, obs-filters): Close Threshold; Open Threshold; Attack; Hold Time; Release
- **Noise Suppression** (`noise_suppress_filter`, obs-filters): Method; Suppression Level
- **Render Delay** (`gpu_delay`, obs-filters): Delay
- **Scaling/Aspect Ratio** (`scale_filter`, obs-filters): Scale Filtering; Resolution; Undistort center of image when scaling from ultrawide
- **Scroll** (`scroll_filter`, obs-filters): speed_x; speed_y; limit_cx; cx; limit_cy; cy; loop
- **Sharpen** (`sharpness_filter`, obs-filters): sdr_only_info; sharpness
- **Upward Compressor** (`upward_compressor_filter`, obs-filters): Presets; Ratio; Threshold; Attack; Release; Output Gain; Detection; Knee Width
- **Video Delay (Async)** (`async_delay_filter`, obs-filters): Delay
- **vst_filter** (`vst_filter`, obs-vst): VST 2.x Plug-in; Open Plug-in Interface; Close Plug-in Interface; Open interface when active

### Transitions

- **Cut** (`cut_transition`, obs-transitions): no options
- **Fade** (`fade_transition`, obs-transitions): no options
- **Fade to Color** (`fade_to_color_transition`, obs-transitions): Color; Peak Color Point
- **Luma Wipe** (`wipe_transition`, obs-transitions): Image; Softness; Invert
- **Slide** (`slide_transition`, obs-transitions): S_DIRECTION
- **Stinger** (`obs_stinger_transition`, obs-transitions): path; tp_type; hw_decode; preload; transition_point; track_matte_layout; track_matte_path; invert_matte; track_matte_enabled; audio_monitoring; audio_fade_style
- **Swipe** (`swipe_transition`, obs-transitions): S_DIRECTION; S_SWIPE_IN

### Encoders

`av1_ffmpeg_vaapi`; `av1_ffmpeg_vaapi_tex`; `ffmpeg_aac`; `ffmpeg_alac`; `ffmpeg_aom_av1`; `ffmpeg_flac`; `ffmpeg_hevc_nvenc`; `ffmpeg_nvenc`; `ffmpeg_openh264`; `ffmpeg_opus`; `ffmpeg_pcm_f32le`; `ffmpeg_pcm_s16le`; `ffmpeg_pcm_s24le`; `ffmpeg_svt_av1`; `ffmpeg_vaapi`; `ffmpeg_vaapi_tex`; `hevc_ffmpeg_vaapi`; `hevc_ffmpeg_vaapi_tex`; `jim_av1_nvenc`; `jim_hevc_nvenc`; `jim_nvenc`; `libfdk_aac`; `obs_nvenc_av1_cuda`; `obs_nvenc_av1_soft`; `obs_nvenc_av1_tex`; `obs_nvenc_h264_cuda`; `obs_nvenc_h264_soft`; `obs_nvenc_h264_tex`; `obs_nvenc_hevc_cuda`; `obs_nvenc_hevc_soft`; `obs_nvenc_hevc_tex`; `obs_qsv11`; `obs_qsv11_av1`; `obs_qsv11_av1_soft`; `obs_qsv11_hevc`; `obs_qsv11_hevc_soft`; `obs_qsv11_soft`; `obs_qsv11_soft_v2`; `obs_qsv11_v2`; `obs_x264`

### Outputs and services

`decklink_output";`; `ffmpeg_hls_muxer`; `ffmpeg_mpegts_muxer`; `ffmpeg_muxer`; `ffmpeg_output`; `flv_output`; `kUIPropOutputModule.id;`; `mov_output`; `mp4_output`; `null_output`; `replay_buffer`; `rtmp_common`; `rtmp_custom`; `rtmp_output`; `virtualcam_output`; `whip_custom";`; `whip_output";`

## Appendix B: every setting, menu and dialog in the OBS app

Generated from `frontend/data/locale/en-US.ini` (labels only; long help text left out).

- **Language**: English
- **OK**: OK
- **Apply**: Apply
- **Cancel**: Cancel
- **Close**: Close
- **Save**: Save
- **Discard**: Discard
- **Disable**: Disable
- **Yes**: Yes
- **No**: No
- **Add**: Add
- **Remove**: Remove
- **Rename**: Rename...
- **Interact**: Interact
- **Filters**: Filters
- **Properties**: Properties
- **MoveUp**: Move Up
- **MoveDown**: Move Down
- **Settings**: Settings
- **Display**: Display
- **Name**: Name
- **Exit**: Exit
- **Mixer**: Audio Mixer
- **Browse**: Browse
- **Mono**: Mono
- **Stereo**: Stereo
- **Projector**: Open Program Projector; Open Preview Projector; Open Scene Projector; Open Source Projector; Open Multiview; New window; Projector; Multiview; Fit window to content
- **Clear**: Clear
- **Revert**: Revert
- **Show**: Show
- **Hide**: Hide
- **UnhideAll**: Unhide All
- **Untitled**: Untitled
- **New**: New...
- **Duplicate**: Duplicate...
- **Enable**: Enable
- **DisableOSXVSync**: Disable macOS V-Sync
- **ResetOSXVSyncOnExit**: Reset macOS V-Sync on Exit
- **Transition**: Transition
- **QuickTransitions**: Quick Transitions; Swap Preview/Program Scenes After Transitioning; Duplicate Scene; Duplicate Sources
- **FadeToBlack**: Fade to Black
- **Left**: Left
- **Right**: Right
- **Top**: Top
- **Bottom**: Bottom
- **Reset**: Reset
- **Hours**: Hours
- **Minutes**: Minutes
- **Seconds**: Seconds
- **Deprecated**: Deprecated
- **ReplayBuffer**: Replay Buffer
- **Import**: Import
- **Export**: Export
- **Copy**: Copy; Copy Filters
- **Paste**: Paste; Paste Filters
- **PasteReference**: Paste (Reference)
- **PasteDuplicate**: Paste (Duplicate)
- **RemuxRecordings**: Remux Recordings
- **Next**: Next
- **Back**: Back
- **Defaults**: Defaults
- **RestoreDefaults**: Defaults
- **HideMixer**: Hide in Mixer
- **TransitionOverride**: Transition Override
- **ShowTransition**: Show Transition
- **HideTransition**: Hide Transition
- **None**: None
- **StudioMode**: Preview; Program
- **ShowInMultiview**: Show in Multiview
- **VerticalLayout**: Vertical Layout
- **Group**: Group
- **DoNotShowAgain**: Do not show again
- **Default**: (Default)
- **Calculating**: Calculating...
- **Fullscreen**: Fullscreen
- **Windowed**: Windowed
- **RefreshBrowser**: Refresh
- **LockVolume**: Lock Volume
- **LogViewer**: Log Viewer
- **ShowOnStartup**: Show on startup
- **OpenFile**: Open file
- **AddScene**: Add Scene
- **AddSource**: Add Source
- **RemoveScene**: Remove Selected Scene
- **RemoveSource**: Remove Selected Source(s)
- **MoveSceneUp**: Move Scene Up
- **MoveSceneDown**: Move Scene Down
- **MoveSourceUp**: Move Source(s) Up
- **MoveSourceDown**: Move Source(s) Down
- **SourceProperties**: Open Source Properties
- **SourceFilters**: Open Source Filters
- **MixerToolbarMenu**: Audio Mixer Menu
- **SceneFilters**: Open Scene Filters
- **List**: List
- **Grid**: Grid
- **Automatic**: Automatic
- **ComingSoon**: Coming Soon
- **PluginsFailedToLoad**: Plugin Load Error
- **AlreadyRunning**: OBS is already running; Launch Anyway
- **CrashHandling**: OBS Studio Crash Detected; I have read the privacy policy and consent to the upload.; Run in Safe Mode; Run in Normal Mode; Crash Log Upload Error
- **SafeMode**: Do you want to restart OBS in Normal Mode?
- **ChromeOS**: Unsupported Platform
- **Wine**: Wine detected
- **DockCloseWarning**: Closing Dockable Window
- **ExtraBrowsers**: Custom Browser Docks; Dock Name
- **Auth**: Authenticating...; Authentication Failure; Authentication Required; Loading channel information...; Couldn't get channel information.; Failed to load channel; Chat; Stream Information
- **TwitchAuth**: Twitch Stats; Twitch Activity Feed; Could not query stream key
- **RestreamAuth**: Restream Channels
- **BrowserPanelInit**: Initializing Browser...; Initializing browser, please wait...
- **BandwidthTest**: Region; United States; Europe; Asia; Other
- **Basic**: Auto-Configuration Wizard; Stats; Add Configurable Transition; Remove Configurable Transition; Transition Properties; Scene Transitions; Duration; Studio Mode; Enable Studio Mode; Disable Studio Mode; Desktop Audio; Desktop Audio 2; Mic/Aux; Mic/Aux 2; Mic/Aux 3; Mic/Aux 4; Scene; Display Capture; Create/Select Source; Filters; Scene Item Transform; Advanced Audio Properties; Plugin Manager
- **AutoConfig.ApplySettings**: Apply Settings
- **AutoConfig.StartPage**: Usage Information; Specify what you want to use the program for; Optimize for streaming, recording is secondary; Optimize just for recording, I will not be streaming; I will only be using the virtual camera
- **AutoConfig.VideoPage**: Video Settings; Specify the video settings you would like to use; Either 60 or 30, but prefer 60 when possible; Either 60 or 30, but prefer high resolution
- **AutoConfig.StreamPage**: Stream Information; Please enter your stream information; Connect Account (recommended); Disconnect Account; Disconnect Account?; Get Stream Key; More Info; Use Stream Key; Use Stream Key (advanced); Service; Show All...; Custom...; Server; Stream Key; Encoder Key; Bearer Token; Connected account; Prefer hardware encoding; Stream warning
- **AutoConfig.TestPage**: Final Results; Testing complete; Performing bandwidth test, this may take a few minutes...; No output for the protocol of this service was found; Testing stream encoder, this may take a minute...; Testing recording encoder, this may take a minute...; Failed to start up encoder; Streaming Encoder; Recording Encoder; Streaming (Scaled) Resolution
- **AutoConfig.RunAnytime**: This can be run at any time by going to the Tools menu.
- **Stats.CPUUsage**: CPU Usage
- **Stats.HDDSpaceAvailable**: Disk space available
- **Stats.MemoryUsage**: Memory Usage
- **Stats.AverageTimeToRender**: Average time to render frame
- **Stats.SkippedFrames**: Skipped frames due to encoding lag
- **Stats.MissedFrames**: Frames missed due to rendering lag
- **Stats.Output**: Stream; Recording
- **Stats.Status**: Status; Recording; LIVE; Reconnecting; Inactive; Active
- **Stats.DroppedFrames**: Dropped Frames (Network)
- **Stats.MegabytesSent**: Total Data Output
- **Stats.Bitrate**: Bitrate
- **Stats.DiskFullIn**: Disk full in (approx.)
- **Stats.ResetStats**: Reset Stats
- **ResetUIWarning**: Are you sure you want to reset the UI?
- **Updater**: New update available; There is a new update available:; Update Now; Remind me Later; Skip Version; No updates available; No updates are currently available; Update Channel Removed; Integrity Check Unavailable; Confirm Integrity Check; Failed to launch updater
- **Undo**: Undo; Redo
- **TransitionNameDlg**: Please enter the name of the transition; Transition Name
- **TitleBar**: SAFE MODE; Portable Mode; Profile; Scenes
- **NameExists**: Name already exists; The name is already in use.
- **NoNameEntered**: Please enter a valid name; You cannot use empty names.
- **ConfirmStart**: Start Stream?; Are you sure you want to start the stream?
- **ConfirmStop**: Stop Stream?; Are you sure you want to stop the stream?
- **ConfirmStopRecord**: Stop Recording?; Are you sure you want to stop recording?
- **ConfirmBWTest**: Start Bandwidth Test?
- **ConfirmExit**: Exit OBS?
- **ConfirmRemove**: Confirm Remove
- **ConfirmReset**: Reset Properties
- **Output**: Failed to start streaming; Failed to start recording; Failed to start replay buffer; Failed to start virtual camera; Cannot save replays while paused; Warning: Replays cannot be saved while recording is paused.; Failed to connect; Failed to connect to server; HDR output is currently disabled for this output.; Disconnected from server.; Encoding error; An encoder error occurred while streaming.; Insufficient disk space; There is not sufficient disk space to continue recording.; Recording error; An unspecified error occurred while recording.; An encoder error occurred while recording.; Bad File Path; No Broadcast Configured; Failed to start broadcast; Failed to stop broadcast
- **LogUploadDialog**: OBS Studio Log File Upload; Log upload in progress. Please wait...; Upload; Copy Log URL; Analyze Log File; Retry; No file to upload found or file was empty.
- **Remux**: OBS Recording; Target File; Remux; Stop Remuxing; Clear Finished Items; Clear All Items; Remuxing finished; Recording remuxed; Recording remuxed, but the file may be incomplete; Select OBS Recording...; Select target file...; Target files exist; Remuxing in progress; No remuxing file added
- **MissingFiles**: Missing Files; Missing File; New File; Some files are missing since you last used OBS.; Search Directory...; Select file...; Select Folder to Search in; State; Missing; Replaced; Cleared; Found; Additional file matches found; Missing Files Check; No files appear to be missing.
- **MacPermissions**: Review App Permissions...; Review App Permissions; You can re-open this dialog via the OBS Studio menu.; Access Granted; Request Access; Screen Recording; Camera; Microphone; Input Monitoring; Accessibility; Continue
- **SourceLeak**: Source Cleanup Error
- **Main.PreviewConextMenu**: Enable Preview
- **Main.Preview**: Disable Preview
- **ScaleFiltering**: Scale Filtering; Point; Bilinear; Bicubic; Lanczos; Area
- **BlendingMethod**: Blending Method; Default; SRGB Off
- **BlendingMode**: Blending Mode; Normal; Add; Subtract; Screen; Multiply; Lighten; Darken
- **Deinterlacing**: Deinterlacing; Discard; Retro; Blend; Blend 2x; Linear; Linear 2x; Yadif; Yadif 2x; Top Field First; Bottom Field First
- **VolControl**: Unassigned Audio Source
- **Main.AddSceneDlg**: Add Scene; Please enter the name of the scene
- **Main.AddSceneCollection**: Add Scene Collection; Please enter the name of the scene collection
- **Main.RenameSceneCollection**: Rename Scene Collection
- **Main.RemigrateSceneCollection**: Update Scene Collection Resolution; Reset not possible: Changing OBS resolution failed.
- **AddProfile**: Add Profile; Please enter the name of the profile; Show auto-configuration wizard
- **RenameProfile**: Rename Profile
- **Main.MixerRename**: Rename Audio Source; Please enter the name of the audio source
- **Main.PreviewDisabled**: Preview is currently disabled
- **SourceSelect.CreateNew**: Create new
- **SourceSelect.AddExisting**: Add Existing
- **SourceSelect.AddVisible**: Make source visible
- **Main.Sources**: Visibility; Lock; Sources
- **PropertiesWindow.SelectColor**: Select color
- **PropertiesWindow.SelectFont**: Select font; Pick a Font
- **PropertiesWindow.ConfirmTitle**: Settings Changed
- **PropertiesWindow.Confirm**: There are unsaved changes. Do you want to keep them?
- **PropertiesWindow.NoProperties**: No properties available
- **PropertiesWindow.AddFiles**: Add Files
- **PropertiesWindow.AddDir**: Add Directory
- **PropertiesWindow.AddURL**: Add Path/URL
- **PropertiesView.FPS**: Simple FPS Values; Rational FPS Values; Valid FPS Ranges:
- **PropertiesView.UrlButton**: Open this link in your default web browser?; Open URL
- **StatusBar.ReconnectSuccessful**: Reconnection successful
- **Filters.AsyncFilters**: Audio/Video Filters
- **Filters.AudioFilters**: Audio Filters
- **Filters.EffectFilters**: Effect Filters
- **Filters.AddFilter**: Filter name; Please specify the name of the filter
- **TransformWindow.Position**: Position
- **TransformWindow.PositionX**: Position X
- **TransformWindow.PositionY**: Position Y
- **TransformWindow.Rotation**: Rotation
- **TransformWindow.Size**: Size
- **TransformWindow.Width**: Width
- **TransformWindow.Height**: Height
- **TransformWindow.Alignment**: Positional Alignment; Top Left; Top Center; Top Right; Center Left; Center; Center Right; Bottom Left; Bottom Center; Bottom Right
- **TransformWindow.BoundsType**: Bounding Box Type; No bounds; Maximum size only; Scale to inner bounds; Scale to outer bounds; Scale to width of bounds; Scale to height of bounds; Stretch to bounds
- **TransformWindow.BoundsAlignment**: Alignment in Bounding Box
- **TransformWindow.Bounds**: Bounding Box Size
- **TransformWindow.BoundsWidth**: Bounding Box Width
- **TransformWindow.BoundsHeight**: Bounding Box Height
- **TransformWindow.CropToBounds**: Crop to Bounding Box
- **TransformWindow.Crop**: Crop
- **TransformWindow.CropLeft**: Crop Left
- **TransformWindow.CropRight**: Crop Right
- **TransformWindow.CropTop**: Crop Top
- **TransformWindow.CropBottom**: Crop Bottom
- **TransformWindow.NoSelectedSource**: No source selected
- **Main.AddSourceHelp**: Cannot Add Source; You need to have at least 1 scene to add a source.
- **Main.Scenes**: Scenes
- **Main.Source**: Source
- **Main.Controls**: Controls
- **Main.PreparingStream**: Preparing...
- **Main.Connecting**: Connecting...
- **Main.StartRecording**: Start Recording
- **Main.StartReplayBuffer**: Start Replay Buffer
- **Main.SaveReplay**: Save Replay
- **Main.StartStreaming**: Start Streaming
- **Main.StartBroadcast**: Go Live
- **Main.StartVirtualCam**: Start Virtual Camera
- **Main.StopRecording**: Stop Recording
- **Main.PauseRecording**: Pause Recording
- **Main.UnpauseRecording**: Unpause Recording
- **Main.SplitFile**: Split Recording File
- **Main.AddChapterMarker**: Add Chapter Marker (Hybrid MP4/MOV only)
- **Main.StoppingRecording**: Stopping Recording...
- **Main.StopReplayBuffer**: Stop Replay Buffer
- **Main.StoppingReplayBuffer**: Stopping Replay Buffer...
- **Main.SetupBroadcast**: Manage Broadcast
- **Main.StopStreaming**: Stop Streaming
- **Main.StopBroadcast**: End Broadcast
- **Main.AutoStopEnabled**: (Auto Stop)
- **Main.StoppingStreaming**: Stopping Stream...
- **Main.ForceStopStreaming**: Stop Streaming (discard delay)
- **Main.ShowContextBar**: Show Source Toolbar
- **Main.HideContextBar**: Hide Source Toolbar
- **Main.StopVirtualCam**: Stop Virtual Camera
- **Main.GroupItems**: Group Selected Items
- **Main.Ungroup**: Ungroup
- **Main.GridMode**: Grid Mode
- **Main.ListMode**: List Mode
- **Main.VirtualCamConfig**: Configure Virtual Camera
- **VCam.VirtualCamera**: Virtual Camera
- **VCam.OutputType**: Output Type; Program (Default)
- **VCam.OutputSelection**: Output Selection; No selection for this output type
- **VCam.RestartWarning**: The virtual camera will be restarted to apply this change
- **MainMenu.File**: &File; &Export; &Import; Show &Recordings; Re&mux Recordings; &Settings; Show Settings Folder; Show Profile Folder; Check for Missing Files; E&xit
- **MainMenu.Edit**: &Edit; &Undo; &Redo; &Lock Preview; Preview &Scaling; Scale to Window; Zoom In; Zoom Out; Reset Zoom; &Transform; &Edit Transform; Copy Transform; Paste Transform; &Reset Transform; Rotate 90 degrees CW; Rotate 90 degrees CCW; Rotate 180 degrees; Flip &Horizontal; Flip &Vertical; &Fit to screen; &Stretch to screen; &Center to screen; Center Vertically; Center Horizontally; &Order; Move &Up; Move &Down; Move to &Top; Move to &Bottom; &Advanced Audio Properties
- **MainMenu.View**: &View; &Toolbars; Dock Toolbars; Source Toolbar; Source &Icons; &Status Bar; Fullscreen Interface; &Reset UI; &Always On Top; Scene List Mode
- **MainMenu.Docks**: &Docks; &Reset Docks; &Lock Docks; &Full-Height Docks; &Custom Browser Docks
- **MainMenu.Import**: Import...
- **MainMenu.Export**: Export...
- **MainMenu.SceneCollection**: &Scene Collection; Import Scene Collection; Export Scene Collection; Reset Base Resolution; Set Base Resolution
- **MainMenu.Profile**: &Profile; Import Profile; Export Profile; The profile already exists
- **MainMenu.Tools**: &Tools
- **MainMenu.Help**: &Help; Help &Portal; Visit &Website; Join &Discord Server; What's New; &Log Files; &Show Log Files; Upload &Current Log File; Upload &Previous Log File; &View Current Log; Release Notes; Check For Updates; Check File Integrity; Restart in Safe Mode; Restart in Normal Mode; Crash &Reports; &Show Crash Reports; Upload &Previous Crash Report; &About
- **Settings.ConfirmTitle**: Confirm Changes
- **Settings.Confirm**: You have unsaved changes. Save changes?
- **Settings.General**: General; Language; Updates; Update Channel; (Disabled); (Default); Automatically check for updates on startup; Open stats dialog on startup; Hide OBS windows from screen capture; Show confirmation dialog when starting streams; Show confirmation dialog when stopping streams; Show confirmation dialog when stopping recording; Projectors; Hide cursor over projectors; Make projectors always on top; Source Alignment Snapping; Snap Sources to edge of screen; Snap Sources to horizontal and vertical center; Snap Sources to other sources; Snap Sensitivity; Show pixel alignment guides; Automatically record when streaming; Keep recording when stream stops; Automatically start replay buffer when streaming; Keep replay buffer active when stream stops; System Tray; Minimize to system tray when started; Always minimize to system tray instead of task bar; Save projectors on exit; Limit one fullscreen projector per screen; Preview; Hide overflow; Overflow always visible; Show overflow even when source is invisible; Importers; Search known locations for scene collections when importing; Transition to scene when double-clicked; Enable portrait/vertical layout; Show preview/program labels; Multiview; Click to switch between scenes; Show scene names; Draw safe areas (EBU R 95); Multiview Layout; Horizontal, Top (8 Scenes); Horizontal, Bottom (8 Scenes); Vertical, Left (8 Scenes); Vertical, Right (8 Scenes); Horizontal, Top (18 Scenes); Horizontal, Top (24 Scenes); Scenes only (4 Scenes); Scenes only (9 Scenes); Scenes only (16 Scenes); Scenes only (25 Scenes); Stable; Latest stable release; Betas / Release Candidates; Potentially unstable pre-release versions
- **Settings.Appearance**: Appearance; General; Theme; Style; No Styles Available; Font Size; Density; Classic; Compact; Normal; Comfortable; Some appearance options are not available for this style.
- **Settings.Stream**: Stream; Destination; Use authentication; Username; Password; Enable Bandwidth Test Mode; Twitch Chat Add-Ons; None; BetterTTV; FrankerFaceZ; BetterTTV and FrankerFaceZ; Missing Stream Setup; Open Settings; Ignore streaming service setting recommendations; Override Recommended Settings; Specify Custom Server...; Custom Server; Maximum Streaming Bandwidth; Auto; Maximum Video Tracks; Config Override (JSON); Enable Config Override; Multitrack Video; Additional Canvas; Advanced Options
- **Settings.Output**: Output; Recording Format; Matroska Video (.mkv); Flash Video (.flv); MPEG-4 (.mp4); QuickTime (.mov); MPEG-TS (.ts); HLS (.m3u8 + .ts); Hybrid MP4 (.mp4); Hybrid MOV (.mov); Fragmented MP4 (.mp4); Fragmented MOV (.mov); Video Encoder; Audio Encoder; Select Recording Directory; Dynamically change bitrate to manage congestion; Dynamically change bitrate to manage congestion (Beta); Output Mode; Simple; Advanced; FFmpeg Output; Enable Replay Buffer; Maximum Replay Time; Maximum Memory; Replay Buffer Filename Prefix; Suffix; Recording Path; Recording Quality; Same as stream; High Quality, Medium File Size; Indistinguishable Quality, Large File Size; Lossless Quality, Tremendously Large File Size; Are you sure you want to use lossless quality?; Lossless quality warning!; Software (x264); Hardware (QSV, H.264); Hardware (QSV, AV1); Hardware (AMD, H.264); Hardware (AMD, HEVC); Hardware (AMD, AV1); Hardware (NVENC, H.264); Hardware (NVENC, AV1); Hardware (NVENC, HEVC); Hardware (Apple, H.264); Hardware (Apple, HEVC); Software (x264 low CPU usage preset, increases file size); AAC; AAC (Default); Opus; Twitch VOD Track (Uses Track 2); Audio Track; Incompatible Resolution/Framerate; Incompatible Encoder; Video Bitrate; Audio Bitrate; Automatically Reconnect; Retry Delay; Maximum Retries; Enable Custom Encoder Settings (Advanced); Encoder Preset; Custom Encoder Settings; Custom Muxer Settings; Generate File Name without Space; Rescale Output; Disabled; Streaming; Streaming Settings; Track 1; Track 2; Track 3; Track 4; Track 5; Track 6; Twitch VOD Track; Encoder Settings; Recording; Recording Settings; Recording Type; Type; Standard; Custom Output (FFmpeg); (Use stream encoder); Filename Formatting; Overwrite if file exists; FFmpeg Output Type; Output to URL; Output to File; Common recording formats; All Files; File path or URL
- **Screenshot**: Screenshot Output; Screenshot Selected Source; Save Program Screenshot; Save Preview Screenshot; Save Scene Screenshot; Save Source Screenshot
- **FilenameFormatting**: Year, four digits; Year, last two digits (00-99); Month as a decimal number (01-12); Day of the month, zero-padded (01-31); Hour in 24h format (00-23); Minute (00-59); Second (00-59); Abbreviated weekday name; Full weekday name; Abbreviated month name; Full month name; Hour in 12h format (01-12); AM or PM designation; Time in seconds since UNIX epoch; Year; ISO 8601 offset from UTC in timezone; Timezone name or abbreviation; Frames per second; Base (canvas) resolution; Output (scaled) resolution; Video format
- **Settings.Video**: Video; Video Adapter; Base (Canvas) Resolution; Output (Scaled) Resolution; Downscale Filter; [Resolutions match, no downscaling required]; FPS; Common FPS Values; Integer FPS Value; Fractional FPS Value; Numerator; Denominator; Renderer; Bilinear (Fastest, but blurry if scaling); Bicubic (Sharpened scaling, 16 samples); Lanczos (Sharpened scaling, 36 samples); Area (Weighted sum, 4/6/9 samples)
- **Settings.Audio**: Audio; Sample Rate; Channels; Meters; Decay Rate; Fast; Medium (Type I PPM); Slow (Type II PPM); Peak Meter Type; Sample Peak; True Peak (Higher CPU usage); WARNING: Surround sound audio is enabled.; Enable surround sound audio?; Are you sure you want to enable surround sound audio?; Global Audio Devices; Desktop Audio; Desktop Audio 2; Mic/Auxiliary Audio; Mic/Auxiliary Audio 2; Mic/Auxiliary Audio 3; Mic/Auxiliary Audio 4; Enable Push-to-mute; Push-to-mute delay; Enable Push-to-talk; Push-to-talk delay; [Device not connected or not available]; Disabled; Low Latency Audio Buffering Mode (For Decklink/NDI outputs); WARNING: Low latency audio buffering is enabled.; Enable low latency audio buffering mode?
- **Settings.Accessibility**: Accessibility; Use Different Colors; Source Border (Selection); Source Border (Crop); Source Border (Hover); Mixer Volume Band (-60 to -20dB); Mixer Volume Band (-20 to -9dB); Mixer Volume Band (-9 to 0dB); Mixer Volume Band (-60 to -20dB) (active); Mixer Volume Band (-20 to -9dB) (active); Mixer Volume Band (-9 to 0dB) (active); Color Preset; Default; Custom; Color Blind Alternative
- **Settings.Advanced**: Advanced; Show active outputs warning on exit; Process Priority; High; Above Normal; Normal; Below Normal; Idle; Warning: Rec. 2100 should use a format with more precision.; Color Format; NV12 (8-bit, 4:2:0, 2 planes); I420 (8-bit, 4:2:0, 3 planes); I444 (8-bit, 4:4:4, 3 planes); P010 (10-bit, 4:2:0, 2 planes); I010 (10-bit, 4:2:0, 3 planes); P216 (16-bit, 4:2:2, 2 planes); P416 (16-bit, 4:4:4, 2 planes); BGRA (8-bit); Color Space; sRGB; Rec. 601; Rec. 709; Rec. 2100 (PQ); Rec. 2100 (HLG); Color Range; Limited; Full; SDR White Level; HDR Nominal Peak Level; Monitoring Device; Default; Disable Windows audio ducking; Stream Delay; Duration; Preserve cutoff point (increase delay) when reconnecting; Network; Bind to IP; IP Family; Enable network optimizations; Enable TCP pacing; Hotkey Focus Behavior; Never disable hotkeys; Disable hotkeys when main window is in focus; Disable hotkeys when main window is not in focus; (record as mkv)
- **AdvAudio.ActiveOnly**: Active Sources Only
- **AdvAudio.Name**: Name
- **AdvAudio.Volume**: Volume
- **AdvAudio.Mono**: Mono
- **AdvAudio.Balance**: Balance
- **AdvAudio.SyncOffset**: Sync Offset
- **AdvAudio.Monitoring**: Audio Monitoring; Monitor Off; Monitor Only (mute output); Monitor and Output
- **AdvAudio.AudioTracks**: Tracks
- **Settings.Hotkeys**: Hotkeys; Filter; Filter by Hotkey; Loading hotkeys, please wait...
- **Hotkeys.SelectScene**: Switch to scene
- **SystemTray.Show**: Show
- **SystemTray.Hide**: Hide
- **SystemTray.Message**: Disconnected. Reconnecting...
- **Hotkeys**: Insert; Delete; Home; End; Page Up; Page Down; Num Lock; Scroll Lock; Caps Lock; Backspace; Tab; Print; Pause; Left; Right; Up; Down; Windows; Super; Menu; Space; Numpad Multiply; Numpad Divide; Numpad Add; Numpad Subtract; Numpad Decimal; * (Keypad); / (Keypad); + (Keypad); - (Keypad); . (Keypad); = (Keypad); Esc
- **Mute**: Mute
- **Unmute**: Unmute
- **Push-to-mute**: Push-to-mute
- **Push-to-talk**: Push-to-talk
- **OutputWarnings**: You must select at least one track; Output Settings Error; All outputs must have at least one audio track selected.
- **CodecCompat**: Select Encoder...; Select Format...; No Encoder Selected; No Format Selected
- **FinalScene**: Delete Scene; There needs to be at least one scene.
- **NoSources**: No Sources
- **ChangeBG**: Set Color
- **CustomColor**: Custom Color
- **BrowserSource**: Enable Browser Source Hardware Acceleration
- **About**: About; Make a Contribution; Get Involved; Authors; License; Support the OBS Project
- **AddUrl**: Add Source via URL
- **ResizeOutputSizeOfSource**: Resize output (source size); Do you want to continue?
- **PreviewTransition**: Preview Transition
- **Importer**: Import Scene Collection; Select a Scene Collection; Scene Collection; Collection Path; Detected Application; Automatically Search for Scene Collections; Browse...
- **OBSStudio**: OBS Studio
- **OBSClassic**: OBS Classic
- **Streamlabs**: Streamlabs
- **XSplitBroadcaster**: XSplit Broadcaster
- **Restart**: Restart
- **ContextBar**: No source selected; Play Media; Pause Media; Stop Media; Restart Media; Next in Playlist; Previous in Playlist; Media Seek Widget
- **YouTube**: The authorization process was not completed.; No channel(s) available on selected account; YouTube User Authorization; Create New Broadcast; Select Existing Broadcast; Title*; My Broadcast; Description; Privacy*; Private; Public; Unlisted; Category; Thumbnail; Select file...; No file selected; Clear; Is this video made for kids?*; Yes, it's made for kids; No, it's not made for kids; (?); Additional settings; Latency; Normal; Low; Ultra low; Enable Auto-start; Enable Auto-stop; Enable DVR; 360 video; Schedule for later; Remember these settings; Create broadcast; Create broadcast and start streaming; Select broadcast; Select broadcast and start streaming; Schedule broadcast; Schedule and select broadcast; Open YouTube Studio; Live broadcast creation error; No stream created. Please relink your account.; The selected broadcast was not found.; Selected file does not exist.; Failed opening selected file.; Selected file is too large (Limit: 2 MiB).; Loading list of events...; Event Created; Event created successfully.; Stream; Resume interrupted stream; Automatically created by YouTube Studio; YouTube; Creating a new Live Broadcast, please wait...; Manual start required; Clear Legacy YouTube Browser Docks
- **ConfigDownload**: Warning
- **FailedToStartStream**: No config URL available for the current service; Custom RTMP URL not specified; Invalid custom config; A configured extra canvas is missing; Failed to create multitrack video service; Failed to create multitrack video RTMP output; Failed to create audio encoder; Config does not contain stream target RTMP(S) URL; Do you want to continue streaming?; Go live config did not include encoder configurations; Go live config included an invalid encoder configuration; Go live request returned an unspecified error; Missing config
- **MultitrackVideo**: Incompatible Settings; Disable for this stream and Start Streaming; Update Settings and Start Streaming
- **PluginManager**: Plugin Manager; OBS Studio must be restarted to apply plugin changes; [PLUGIN NOT FOUND]; Browse; Installed; Updates; Manage Enabled Plugins
