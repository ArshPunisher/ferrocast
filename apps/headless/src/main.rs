//! `ferrocast-headless`: the engine and local server with no window, for a spare PC or a
//! Linux server. Open the printed link in a browser to use the dashboard.

use std::path::PathBuf;

fn usage() -> &'static str {
    "ferrocast-headless [--data <dir>] [--port <port>] [--lan]\n\
     \n\
     --data  where Ferrocast keeps its data (default: the user's data folder)\n\
     --port  first port to try (default 47820)\n\
     --lan   accept connections from other devices on the network (a session is still required)"
}

#[tokio::main]
async fn main() {
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();

    let mut dir: Option<PathBuf> = None;
    let mut opts = ferrocast_server::ServeOptions::default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--data" => dir = args.next().map(PathBuf::from),
            "--port" => match args.next().and_then(|p| p.parse().ok()) {
                Some(p) => opts.port = p,
                None => fail("--port needs a number"),
            },
            "--lan" => opts.lan = true,
            "-h" | "--help" => {
                println!("{}", usage());
                return;
            }
            other => fail(&format!("unknown option {other}")),
        }
    }
    let dir = dir
        .or_else(|| std::env::var_os("FERROCAST_HOME").map(PathBuf::from))
        .unwrap_or_else(|| {
            dirs::data_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("Ferrocast")
        });

    let engine = match ferrocast_engine::Engine::start(&dir).await {
        Ok(e) => e,
        Err(e) => fail(&format!("couldn't start: {e}")),
    };
    let server = match ferrocast_server::serve(engine, opts).await {
        Ok(s) => s,
        Err(e) => fail(&format!("the local server couldn't start: {e}")),
    };
    let code = server.sessions.new_code();
    println!("Ferrocast is running at {}", server.base_url());
    println!("Data folder: {}", dir.display());
    println!("Open the dashboard (this link works once, for 2 minutes):");
    println!("  {}/app/#pair={code}", server.base_url());
    if tokio::signal::ctrl_c().await.is_ok() {
        println!("Stopping.");
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("ferrocast-headless: {msg}\n\n{}", usage());
    std::process::exit(2);
}
