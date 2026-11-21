//! LemonMixCtl.exe: `LemonMix.exe --control` as a console program, which a
//! terminal waits for and shows the output of.

#![allow(dead_code)]

#[path = "ctl.rs"]
mod ctl;
#[path = "pipe_client.rs"]
mod pipe_client;
#[path = "protocol.rs"]
mod protocol;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(ctl::run(&args));
}
