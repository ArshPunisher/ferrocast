//! Generates the gRPC client (and a server, for the fake in tests) for YouTube's
//! `liveChatMessages.streamList`. protoc comes vendored, so nothing needs installing.
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = tonic_prost_build::Config::new();
    config.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    let includes = [PathBuf::from("proto"), protoc_bin_vendored::include_path()?];
    tonic_prost_build::configure().build_server(true).compile_with_config(
        config,
        &[PathBuf::from("proto/stream_list.proto")],
        &includes,
    )?;
    println!("cargo:rerun-if-changed=proto/stream_list.proto");
    Ok(())
}
