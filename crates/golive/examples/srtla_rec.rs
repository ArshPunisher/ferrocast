//! An SRTLA receiver: senders' connections put back into one SRT stream to an SRT server.
//!
//!   cargo run -p ferrocast-golive --example srtla_rec -- <listen addr> <srt server addr>
//!   cargo run -p ferrocast-golive --example srtla_rec -- 0.0.0.0:5000 127.0.0.1:8890

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let mut args = std::env::args().skip(1);
    let listen = args.next().unwrap_or_else(|| "0.0.0.0:5000".into());
    let srt: std::net::SocketAddr =
        args.next().unwrap_or_else(|| "127.0.0.1:8890".into()).parse().map_err(std::io::Error::other)?;
    let sock = tokio::net::UdpSocket::bind(&listen).await?;
    println!("SRTLA on {} → SRT {srt}", sock.local_addr()?);
    ferrocast_golive::srtla::receive(sock, srt).await
}
