mod app;
mod assemblers;
mod collectors;
mod config;
mod data;
mod framework;
mod outputs;
mod protocol;

fn main() -> anyhow::Result<()> {
    app::run()
}
