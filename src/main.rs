mod cache;
mod config;
mod engine;
mod git;
mod module_probe;
mod rules;
mod theme;
mod types;

fn main() {
    println!("konform {}", env!("CARGO_PKG_VERSION"));
}
