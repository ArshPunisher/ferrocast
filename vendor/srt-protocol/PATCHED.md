# srt-protocol 0.4.4, patched

A copy of [srt-protocol 0.4.4](https://crates.io/crates/srt-protocol) (Apache-2.0, see
`LICENSE-APACHE`), used through `[patch.crates-io]` in the workspace `Cargo.toml`.

Changes from the published crate (two lines):

- `src/options/srt_version.rs`: `SrtVersion::CURRENT` is 1.4.1 instead of 1.3.1.
- `src/packet/control/srt.rs`: `SrtShakeFlags::SUPPORTED` adds `TLPKTDROP` and `NAKREPORT`.

Why: SRT servers built on gosrt (MediaMTX, datarhei Restreamer) and others refuse a caller older
than 1.4.1 or without those two flags. The crate already drops too-late packets and sends periodic
loss reports; it only didn't say so in the handshake.

Drop this copy when a released srt-protocol makes the same change.
