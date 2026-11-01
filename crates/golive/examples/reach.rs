//! "Test connection" from the command line: tries a destination as Go live would.
//!
//!   cargo run -p ferrocast-golive --example reach -- <url> [key]

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let url = args.next().unwrap_or_default();
    let key = args.next().unwrap_or_default();
    let r = ferrocast_golive::reach::reach(&url, &key).await;
    println!("{}", serde_json::to_string(&r).unwrap_or_default());
}
