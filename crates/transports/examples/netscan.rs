//! Print the network printers the "Find printers" button would show.
//!   cargo run -p zprint-transports --example netscan

#[tokio::main]
async fn main() {
    let started = std::time::Instant::now();
    let found = zprint_transports::netscan::scan().await;
    for p in &found {
        println!("{}{}", if p.is_zebra { "[Zebra] " } else { "        " }, p.label());
    }
    println!("{} found in {:.1}s", found.len(), started.elapsed().as_secs_f32());
}
