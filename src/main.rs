mod engine;
mod mining;
mod storage;
mod stratum;
mod ui;
mod workers;

fn main() -> anyhow::Result<()> {
    ui::run()
}
