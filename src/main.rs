mod engine;
mod mining;
mod stratum;
mod ui;

fn main() -> anyhow::Result<()> {
    ui::run()
}
