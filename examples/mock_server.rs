//! Serves the simulated loopback device used by the tests, for manual experiments:
//! `cargo run --example mock_server -- 127.0.0.1:3240 [speed]`, then attach busid `1-1`.

#[path = "../tests/support/mod.rs"]
mod support;

#[tokio::main]
async fn main() {
    support::init_logging();
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:3240".into());
    let speed: u32 = std::env::args().nth(2).map_or(3, |s| s.parse().unwrap());
    let device = support::device::LoopbackDevice::default();
    let mut export = device.export("1-1");
    export.speed = speed;
    let server = support::server::Server::bind(&addr, vec![export]).await;
    eprintln!("serving 1-1 on {}", server.addr);
    tokio::signal::ctrl_c().await.unwrap();
    for e in server.log.violations() {
        eprintln!("protocol violation: {e}");
    }
}
