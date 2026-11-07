//! "Test connection" from the command line: tries a destination as Go live would.
//!
//!   cargo run -p ferrocast-golive --example reach -- <url> [key]
//!   cargo run -p ferrocast-golive --example reach -- --networks
//!
//! `--networks` lists the connections a failing RTMP stream moves to, in order.

use ferrocast_golive::links;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let url = args.next().unwrap_or_default();
    if url == "--networks" {
        let (list, def) = (links::fallbacks(), links::default_ip());
        println!("default: {def:?}");
        let mut at = None;
        while let Some(n) = links::next_after(at.as_ref(), &list, def) {
            println!("then {} ({}) {}", links::label(&n.name), n.name, n.ip);
            at = Some(n);
        }
        return;
    }
    let key = args.next().unwrap_or_default();
    let r = ferrocast_golive::reach::reach(&url, &key).await;
    println!("{}", serde_json::to_string(&r).unwrap_or_default());
}
