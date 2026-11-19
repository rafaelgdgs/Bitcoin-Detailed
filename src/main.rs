mod engine;
mod mining;
mod storage;
mod stratum;
mod ui;

fn main() -> anyhow::Result<()> {
    ui::run()
}
