mod app;
mod cli;
mod driver;
mod engine;
mod host;
mod metrics;
mod model;
mod probe;
mod runner;
mod stats;
mod store;
mod target;
mod validity;

fn main() {
    std::process::exit(app::main());
}
