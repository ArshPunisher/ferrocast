# Twitch Enhanced Broadcasting (owner: "yes do twitch enhance", 11 Nov 2026)

OBS's "Enhanced Broadcasting": the PC encodes several qualities itself and sends them all in
one stream; Twitch passes them to viewers as they are (no transcoding at Twitch), so every
viewer gets a quality option, also on channels without Twitch's own transcodes.

## How OBS does it (reference/obs-studio/frontend/utility, plugins/obs-outputs/flv-mux.c)

1. **Ask Twitch for a plan.** POST JSON to
   `https://ingest.twitch.tv/api/v3/GetClientConfiguration` (5 s), `GoLiveAPI_PostData.cpp`:
   `{ service: "IVS", schema_version: "2025-01-25", authentication: <stream key>,
   client: { name, version, supported_codecs: ["h264", …] }, capabilities: { cpu, memory, gpu,
   system, gaming_features? }, preferences: { vod_track_audio, canvases: [{ width, height,
   canvas_width, canvas_height, framerate: { numerator, denominator } }],
   maximum_aggregate_bitrate?, maximum_video_tracks?, audio_samples_per_sec, audio_channels } }`.
2. **The answer** (`models/multitrack-video.hpp` `Config`): `meta { service, schema_version,
   config_id }`, `status { result: success | warning | error, html_en_us }`,
   `ingest_endpoints [{ protocol: "RTMP"|"RTMPS", url_template: "…{stream_key}", authentication? }]`,
   `encoder_configurations [{ type, width, height, framerate, settings: { bitrate, keyint_sec,
   profile, bf, … }, canvas_index }]`, `audio_configurations { live: [{ codec, track_id,
   channels, settings }], vod? }`. A warning with configurations: ask whether to go on; an
   error: stop with its message.
3. **Encode each configuration** from the same picture (scaled on the GPU), all at the same
   frame rate and keyframe interval.
4. **One RTMP stream, Enhanced RTMP multitrack video** (`flv-mux.c` `flv_packet_ex`): track 0 as
   a plain Enhanced RTMP video packet (`0x80 | packetType | frameType`, FourCC `avc1`), tracks
   1… as `0x80 | 6 (Multitrack) | frameType`, then `0x00 (OneTrack) | packetType`, FourCC,
   `trackId`. H.264 frames carry the 3-byte composition time (`CodedFrames`), or `CodedFramesX`
   when pts == dts. Sequence starts (avcC) per track too.
5. Stream to the ingest endpoint's URL with the key put into `{stream_key}` (or its own
   `authentication`).

## Ferrocast's plan (Studio → Ingest → Hub → push; docs/plan/LIVE-STUDIO.md)

| Step | What | Where | State |
|---|---|---|---|
| 1 | The config request and answer (models, request from our settings, errors in words) | `crates/golive/src/twitch_eb.rs` | **Built 11 Nov**, tests on a recorded answer |
| 2 | Multitrack video tags (and a reader that tells a track's packets apart) | `crates/golive/src/flv.rs` | **Built 11 Nov**, tests |
| 3 | Extra encoders: the canvas scaled to each configuration, encoded with the hardware encoder, tagged with its track | `crates/ferrocore/src/pipeline.rs` | Next |
| 4 | The hub passes extra tracks; only Twitch's push sends them, the rest drop them (`flv::video_extra_track`). A destination joining mid-stream must start at a main-track keyframe: `video_is_keyframe` is true for extra tracks' keyframes too | `crates/golive/src/push.rs`, `hub.rs` | Next |
| 5 | Go live › Twitch: "Enhanced Broadcasting" switch with (i); its own failures fall back to a normal stream and say why | dashboard, `crates/engine` | Next |
| 6 | Real test: a short Twitch stream with the owner (Mac VideoToolbox; Windows NVENC on the PC) | owner | After 5 |

Limits to keep: Twitch caps total bitrate and track count in its answer; never more tracks
than the encoder can run (VideoToolbox: test 3 at 1080p60); a failure to fetch the plan never
blocks going live (a normal stream, and a note).
