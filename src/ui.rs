use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use bitcoin::consensus::encode::{deserialize, serialize};
use bitcoin::{Address, Network, Script, Transaction, Txid};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph, Tabs, Wrap};
use ratatui::{DefaultTerminal, Frame};
use time::UtcOffset;

use crate::activity::{HISTORY_SECONDS, RECENT_SECONDS};
use crate::engine::{self, Command, Config, Connection, Snapshot};
use crate::gpu::Intensity;
use crate::mining::{self, Hash256, Work};
use crate::storage::{Best, FILE_NAME, SavedState, SetupForm, Storage};
use crate::workers::{Backend, GpuStatus, NONCE_SPACE};

const BG: Color = Color::Rgb(13, 19, 29);
const SURFACE: Color = Color::Rgb(21, 29, 42);
const RAISED: Color = Color::Rgb(29, 40, 56);
const BORDER: Color = Color::Rgb(66, 83, 105);
const TEXT: Color = Color::Rgb(224, 232, 243);
const DIM: Color = Color::Rgb(159, 174, 194);
const GOLD: Color = Color::Rgb(255, 190, 107);
const MINT: Color = Color::Rgb(115, 227, 182);
const RED: Color = Color::Rgb(255, 139, 145);
const SECTION_GAP: u16 = 4;
const CHART_AXIS: u16 = 4;
const CHART_MIN_WIDTH: u16 = HISTORY_SECONDS as u16 + CHART_AXIS;
const CHART_ROW_BITS: u16 = 8;
const CHART_ROWS: u16 = 8;
const CHART_MAX_BITS: u16 = CHART_ROW_BITS * CHART_ROWS;
const TABS: [&str; 7] = [
    "1 Mine",
    "2 Header",
    "3 Coinbase",
    "4 Merkle",
    "5 Transactions",
    "6 Best",
    "7 Device",
];
const COMPACT_TABS: [&str; 7] = ["1", "2", "3", "4", "5", "6", "7"];
const SHORT_TABS: [&str; 7] = [
    "1 Mine",
    "2 Header",
    "3 Coinbase",
    "4 Merkle",
    "5 TXIDs",
    "6 Best",
    "7 Device",
];
const DEVICE_TAB: usize = 6;

type Lines = Vec<Line<'static>>;

pub fn run() -> Result<()> {
    let mut app = App::new()?;
    let mut terminal = ratatui::try_init()?;
    let result = (|| {
        execute!(std::io::stdout(), EnableBracketedPaste)?;
        app.run(&mut terminal)
    })();
    let paste = execute!(std::io::stdout(), DisableBracketedPaste);
    let restore = ratatui::try_restore();
    if let Some(connection) = app.connection.take() {
        connection.stop();
    }
    result?;
    paste?;
    restore?;
    Ok(())
}

enum Prompt {
    Nonce(Arc<Work>, String),
    Threads(String),
    Device(String),
    GpuIntensity(String),
}

impl Prompt {
    fn input_mut(&mut self) -> &mut String {
        match self {
            Self::Nonce(_, input)
            | Self::Threads(input)
            | Self::Device(input)
            | Self::GpuIntensity(input) => input,
        }
    }
}

struct App {
    form: SetupForm,
    storage: Storage,
    initial_best: Option<Best>,
    initial_gpu_intensity: Intensity,
    field: usize,
    connection: Option<Connection>,
    tab: usize,
    scroll: u16,
    binary: bool,
    prompt: Option<Prompt>,
    notice: String,
    tx_index: usize,
    selected_job: Option<String>,
    explorer: Option<(Txid, mpsc::Receiver<Result<Transaction>>)>,
    tx_detail: Option<Lines>,
}

impl App {
    fn new() -> Result<Self> {
        let storage = Storage::for_current_exe()?;
        let (state, notice) = match storage.load() {
            Ok(state) => (state, String::new()),
            Err(error) => (
                SavedState::default(),
                format!("Could not load saved data: {error:#}"),
            ),
        };
        Ok(Self {
            form: state.form,
            initial_best: state.best,
            initial_gpu_intensity: state.gpu_intensity,
            storage,
            field: 1,
            connection: None,
            tab: 0,
            scroll: 0,
            binary: false,
            prompt: None,
            notice,
            tx_index: 0,
            selected_job: None,
            explorer: None,
            tx_detail: None,
        })
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            let snapshot = self
                .connection
                .as_ref()
                .map(|conn| conn.snapshot.lock().unwrap().clone());
            self.update_tx_view(snapshot.as_ref());
            terminal.draw(|frame| self.draw(frame, snapshot.as_ref()))?;
            if !event::poll(Duration::from_millis(100))? {
                continue;
            }
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        return Ok(());
                    }
                    if self.key(key, snapshot.as_ref()) {
                        return Ok(());
                    }
                }
                Event::Paste(text) => {
                    if let Some(prompt) = &mut self.prompt {
                        append_input(prompt.input_mut(), &text, 32);
                    } else if self.connection.is_none() {
                        append_input(&mut self.form[self.field], &text, 256);
                    }
                }
                _ => {}
            }
        }
    }

    fn update_tx_view(&mut self, snapshot: Option<&Snapshot>) {
        let current_job = snapshot
            .and_then(|s| s.work.as_ref())
            .map(|work| &work.job.id);
        if current_job != self.selected_job.as_ref() {
            self.selected_job = current_job.cloned();
            self.tx_index = 0;
            self.tx_detail = None;
            if self.tab == 4 {
                self.scroll = 0;
            }
        }
        if let Some((txid, receiver)) = &self.explorer {
            let result = match receiver.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Err(anyhow::anyhow!("Lookup was interrupted"))
                }
            };
            if snapshot
                .and_then(|s| s.txids.as_ref())
                .and_then(|ids| ids.get(self.tx_index))
                == Some(txid)
            {
                match result {
                    Ok(tx) => {
                        let mut lines = vec![
                            label("Esplora · TXID verified"),
                            Line::raw(
                                "The TXID covers the transaction without witness data. Esplora supplies the witness data shown here.",
                            ),
                        ];
                        lines.extend(transaction(&tx, None));
                        lines.push(label("Serialized transaction"));
                        bytes(&mut lines, &serialize(&tx), 0);
                        self.tx_detail = Some(lines);
                        self.scroll = 0;
                        self.notice.clear();
                    }
                    Err(error) => self.notice = format!("Esplora: {error:#}"),
                }
            } else if self.notice == "Looking up transaction in Esplora…" {
                self.notice.clear();
            }
            self.explorer = None;
        }
    }

    fn key(&mut self, key: KeyEvent, snapshot: Option<&Snapshot>) -> bool {
        if let Some(prompt) = &mut self.prompt {
            match key.code {
                KeyCode::Esc => {
                    self.prompt = None;
                    self.notice.clear();
                }
                KeyCode::Enter => {
                    let command = match prompt {
                        Prompt::Nonce(work, input) => mining::parse_nonce(input)
                            .map(|nonce| Command::Manual(work.clone(), nonce))
                            .context("Enter an integer from 0 to 4294967295 or 0x00000000 to 0xffffffff."),
                        Prompt::Threads(input) => engine::parse_thread_count(input).map(Command::SetThreads),
                        Prompt::GpuIntensity(input) => input
                            .trim()
                            .parse()
                            .ok()
                            .and_then(Intensity::new)
                            .map(Command::SetGpuIntensity)
                            .context("Enter a GPU intensity from 1 to 100."),
                        Prompt::Device(input) => {
                            let count = snapshot
                                .and_then(|snapshot| snapshot.gpu.adapters.as_deref())
                                .map_or(0, <[_]>::len);
                            match input.trim().parse::<usize>() {
                                Ok(0) => Ok(Command::SelectBackend(Backend::Cpu)),
                                Ok(number) if number <= count => {
                                    Ok(Command::SelectBackend(Backend::Vulkan(number - 1)))
                                }
                                _ => Err(anyhow::anyhow!("Enter 0 for CPU or a listed GPU number.")),
                            }
                        }
                    };
                    match command {
                        Ok(command) => {
                            self.prompt = None;
                            self.notice.clear();
                            self.send(command);
                        }
                        Err(error) => self.notice = error.to_string(),
                    }
                }
                KeyCode::Backspace => {
                    prompt.input_mut().pop();
                }
                KeyCode::Char(c) => {
                    let allowed = match prompt {
                        Prompt::Nonce(..) => c.is_ascii_hexdigit() || matches!(c, 'x' | 'X'),
                        Prompt::Threads(_) | Prompt::Device(_) | Prompt::GpuIntensity(_) => {
                            c.is_ascii_digit()
                        }
                    };
                    let input = prompt.input_mut();
                    if allowed && input.len() < 32 {
                        input.push(c);
                    }
                }
                _ => {}
            }
            return false;
        }

        let Some(snapshot) = snapshot else {
            match key.code {
                KeyCode::Esc => return true,
                KeyCode::Tab | KeyCode::Down => self.field = (self.field + 1) % self.form.len(),
                KeyCode::BackTab | KeyCode::Up => {
                    self.field = (self.field + self.form.len() - 1) % self.form.len()
                }
                KeyCode::Backspace => {
                    self.form[self.field].pop();
                }
                KeyCode::Enter => {
                    let [pool, address, worker, password, threads] = &self.form;
                    match Config::new(pool, address, worker, password, threads) {
                        Ok(config) => {
                            self.form[4] = config.threads.to_string();
                            let (best, intensity) = match self.storage.save_settings(&self.form) {
                                Ok(state) => {
                                    self.notice.clear();
                                    (state.best, state.gpu_intensity)
                                }
                                Err(error) => {
                                    self.notice = format!("Could not save {FILE_NAME}: {error:#}");
                                    (self.initial_best.take(), self.initial_gpu_intensity)
                                }
                            };
                            self.connection =
                                Some(engine::start(config, self.storage.clone(), best, intensity));
                        }
                        Err(error) => self.notice = format!("{error}"),
                    }
                }
                KeyCode::Char(c) if !c.is_control() && self.form[self.field].len() < 256 => {
                    self.form[self.field].push(c)
                }
                _ => {}
            }
            return false;
        };

        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Tab => self.select_tab((self.tab + 1) % TABS.len()),
            KeyCode::BackTab => self.select_tab((self.tab + TABS.len() - 1) % TABS.len()),
            KeyCode::Char(c @ '1'..='7') => self.select_tab((c as u8 - b'1') as usize),
            KeyCode::Char(' ') => {
                self.notice.clear();
                self.send(Command::Toggle);
            }
            KeyCode::Char('b') => {
                self.binary = !self.binary;
                self.scroll = 0;
            }
            KeyCode::Char('n') => {
                if let Some(work) = &snapshot.work {
                    self.select_tab(0);
                    self.prompt = Some(Prompt::Nonce(work.clone(), String::new()));
                }
            }
            KeyCode::Char('c' | 'C') => {
                self.notice.clear();
                self.prompt = Some(Prompt::Threads(String::new()));
            }
            KeyCode::Char('g' | 'G') => {
                self.select_tab(DEVICE_TAB);
                self.notice.clear();
                self.prompt = Some(Prompt::Device(String::new()));
            }
            KeyCode::Char('i' | 'I') => {
                self.select_tab(DEVICE_TAB);
                self.notice.clear();
                self.prompt = Some(Prompt::GpuIntensity(String::new()));
            }
            KeyCode::Char('t') => {
                if let Some(work) = &snapshot.work {
                    self.select_tab(4);
                    self.send(Command::Txids(work.clone()));
                }
            }
            KeyCode::Esc if self.tab == 4 => {
                self.tx_detail = None;
                self.scroll = 0;
                self.notice.clear();
            }
            KeyCode::Up if self.tx_list_open(snapshot) => {
                self.tx_index = self.tx_index.saturating_sub(1)
            }
            KeyCode::Down if self.tx_list_open(snapshot) => {
                let len = snapshot.txids.as_ref().unwrap().len();
                self.tx_index = (self.tx_index + 1).min(len.saturating_sub(1));
            }
            KeyCode::Enter if self.tx_list_open(snapshot) => {
                if let Some(txid) = snapshot
                    .txids
                    .as_ref()
                    .and_then(|ids| ids.get(self.tx_index))
                {
                    self.fetch(*txid);
                }
            }
            KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Down => self.scroll = self.scroll.saturating_add(1),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(12),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(12),
            KeyCode::Home => self.scroll = 0,
            _ => {}
        }
        false
    }

    fn tx_list_open(&self, snapshot: &Snapshot) -> bool {
        self.tab == 4 && self.tx_detail.is_none() && snapshot.txids.is_some()
    }

    fn select_tab(&mut self, tab: usize) {
        self.tab = tab;
        self.scroll = 0;
    }

    fn send(&mut self, command: Command) {
        if let Some(connection) = &self.connection
            && connection.commands.send(command).is_err()
        {
            self.notice = "The Stratum session ended.".into();
        }
    }

    fn fetch(&mut self, txid: Txid) {
        if self.explorer.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        self.explorer = Some((txid, receiver));
        self.notice = "Looking up transaction in Esplora…".into();
        std::thread::spawn(move || {
            let _ = sender.send(fetch_transaction(txid));
        });
    }

    fn draw(&mut self, frame: &mut Frame, snapshot: Option<&Snapshot>) {
        let screen = frame.area();
        frame.render_widget(Block::new().style(Style::new().fg(TEXT).bg(BG)), screen);
        let area = screen.inner(ratatui::layout::Margin::new(
            u16::from(screen.width >= 76),
            0,
        ));
        if area.width < 36 || area.height < 13 {
            frame.render_widget(
                Paragraph::new("Enlarge the terminal (minimum 36 × 13)."),
                area,
            );
            return;
        }
        let Some(snapshot) = snapshot else {
            self.draw_form(frame, area);
            return;
        };
        let [title, state, tabs, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(if area.height >= 19 { 2 } else { 1 }),
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .areas(area);
        frame.render_widget(
            Line::from(vec![
                Span::styled(" ₿ ", Style::new().fg(BG).bg(GOLD).bold()),
                Span::styled(" BITCOIN DETAILED", Style::new().fg(TEXT).bold()),
                Span::styled("  ·  Mainnet", Style::new().fg(DIM)),
            ])
            .style(Style::new().bg(RAISED)),
            title,
        );
        let status_color = match &snapshot.work {
            Some(work) if work.paid_sats > 0 => MINT,
            Some(_) => GOLD,
            None if snapshot.status.starts_with("Disconnected") => RED,
            None => DIM,
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(" ● ", Style::new().fg(status_color)),
                    Span::styled(snapshot.status.as_str(), Style::new().fg(status_color)),
                ]),
                Line::styled(
                    format!("   Stratum V1 · {}", self.form[0]),
                    Style::new().fg(DIM),
                ),
            ])
            .style(Style::new().bg(SURFACE)),
            state,
        );
        let titles = [TABS, SHORT_TABS, COMPACT_TABS]
            .into_iter()
            .find(|labels| {
                labels.iter().map(|label| label.len() + 2).sum::<usize>() + labels.len() - 1
                    <= usize::from(tabs.width)
            })
            .unwrap_or(COMPACT_TABS);
        let [nav, _] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(tabs);
        frame.render_widget(
            Tabs::new(titles)
                .select(self.tab)
                .highlight_style(Style::new().fg(BG).bg(GOLD).bold())
                .style(Style::new().fg(DIM))
                .divider(" "),
            nav,
        );
        let border = panel()
            .title(format!(" {} ", TABS[self.tab].split_once(' ').unwrap().1))
            .title_style(Style::new().fg(TEXT).bold())
            .padding(Padding::horizontal(u16::from(body.width >= 76)));
        let mut inner = border.inner(body);
        frame.render_widget(border, body);
        let show_metrics = self.tab == 0 && inner.width >= 80 && inner.height >= 19;
        if show_metrics {
            let [metrics, content] =
                Layout::vertical([Constraint::Length(5), Constraint::Fill(1)]).areas(inner);
            draw_metrics(frame, metrics, snapshot);
            inner = content;
        }
        let content = match self.tab {
            0 => self.dashboard(snapshot, inner.width, show_metrics),
            1 => self.header(snapshot),
            2 => self.coinbase(snapshot),
            3 => self.merkle(snapshot),
            4 => self.transactions(snapshot, inner.height),
            5 => self.best(snapshot, inner.width),
            DEVICE_TAB => self.device(snapshot),
            _ => unreachable!(),
        };
        let paragraph = Paragraph::new(content).wrap(Wrap { trim: false });
        let bottom = paragraph
            .line_count(inner.width)
            .saturating_sub(inner.height as usize);
        self.scroll = self.scroll.min(bottom.min(u16::MAX as usize) as u16);
        frame.render_widget(paragraph.scroll((self.scroll, 0)), inner);
        let prompt = match &self.prompt {
            Some(Prompt::Nonce(_, input)) => {
                format!(" Nonce in decimal or 0x…  › {input}▌  [Enter test · Esc cancel]")
            }
            Some(Prompt::Threads(input)) => format!(
                " CPU threads (1–{}, current {})  › {input}▌  [Enter apply · Esc cancel]",
                engine::max_threads(),
                snapshot.threads
            ),
            Some(Prompt::Device(input)) => {
                format!(" Device number (0 = CPU)  › {input}▌  [Enter select · Esc cancel]")
            }
            Some(Prompt::GpuIntensity(input)) => format!(
                " GPU intensity (1–100%, current {}%)  › {input}▌  [Enter apply · Esc cancel]",
                snapshot.gpu.intensity.percent()
            ),
            None if area.width < 75 => match self.tab {
                0 => " Space mine/pause · N nonce · B hex/bin · Tab tabs · Q quit",
                4 => " T TXIDs · ↑↓ scroll · Tab tabs · Q quit",
                DEVICE_TAB => " G device · I intensity · Space mine/pause · Tab tabs · Q quit",
                _ => " ↑↓ scroll · B hex/bin · Tab tabs · Q quit",
            }.into(),
            None => {
                " Space mine/pause · 1–7/Tab switch · N nonce · B hex/bin · G device · I intensity · C threads · T TXIDs · Q quit".into()
            }
        };
        let message = if self.notice.is_empty() {
            &snapshot.message
        } else {
            &self.notice
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(prompt, Style::new().fg(GOLD).bg(RAISED)),
                Line::styled(
                    format!(" {message}"),
                    Style::new().fg(if self.notice.is_empty() { DIM } else { GOLD }),
                ),
            ])
            .wrap(Wrap { trim: false }),
            footer,
        );
    }

    fn draw_form(&self, frame: &mut Frame, area: Rect) {
        let centered = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(area.width.min(96)),
            Constraint::Fill(1),
        ])
        .areas::<3>(area)[1];
        let mut lines = vec![
            Line::styled("Solo Bitcoin mining · Stratum V1", Style::new().fg(DIM)),
            Line::default(),
        ];
        let thread_label = format!("CPU threads (1–{})", engine::max_threads());
        for (i, name) in [
            "Stratum V1 TCP pool",
            "Public Bitcoin address",
            "Worker (optional)",
            "Pool password",
            &thread_label,
        ]
        .iter()
        .enumerate()
        {
            let value = if i == 3 {
                "•".repeat(self.form[i].chars().count())
            } else {
                self.form[i].clone()
            };
            let active = self.field == i;
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{} {name}  › ", if active { "▸" } else { " " }),
                    Style::new().fg(if active { GOLD } else { DIM }),
                ),
                Span::styled(
                    format!("{value}{}", if active { "▌" } else { "" }),
                    if active {
                        Style::new().fg(TEXT).bg(RAISED).bold()
                    } else {
                        Style::new().fg(TEXT)
                    },
                ),
            ]));
        }
        lines.push(Line::default());
        lines.push(Line::styled(
            "Tab/↑↓ select field · Enter connect · Esc quit",
            Style::new().fg(GOLD),
        ));
        if !self.notice.is_empty() {
            lines.push(Line::styled(self.notice.clone(), Style::new().fg(RED)));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .block(
                    panel()
                        .title(label(" ₿  BITCOIN DETAILED "))
                        .padding(Padding::horizontal(1)),
                )
                .wrap(Wrap { trim: false }),
            centered,
        );
    }

    fn dashboard(&self, snapshot: &Snapshot, width: u16, show_metrics: bool) -> Lines {
        let backend = match snapshot.backend {
            Backend::Cpu => format!("Threads: {}", snapshot.threads),
            Backend::Vulkan(index) => snapshot.gpu.adapters.as_ref().unwrap()[index].clone(),
        };
        let mut lines = vec![Line::from(vec![
            Span::styled(
                match (snapshot.running, snapshot.backend) {
                    (true, Backend::Cpu) => " MINING ON CPU ",
                    (true, Backend::Vulkan(_)) => " MINING ON GPU ",
                    (false, _) => " MINING PAUSED ",
                },
                Style::new()
                    .fg(BG)
                    .bg(if snapshot.running { MINT } else { GOLD })
                    .bold(),
            ),
            Span::raw(format!("  {backend}")),
        ])];
        if !show_metrics {
            lines.push(Line::raw(format!(
                "{} · {} attempts · {} shares accepted / {} rejected · {} block candidates",
                hashrate_text(snapshot.rate),
                grouped_digits(&snapshot.total.to_string()),
                snapshot.accepted,
                snapshot.rejected,
                snapshot.network_hits
            )));
        }
        if let Some(candidate) = &snapshot.candidate {
            lines.push(Line::styled(candidate.clone(), Style::new().fg(MINT)));
        }
        let now = Instant::now();
        let Some(work) = &snapshot.work else {
            lines.push(Line::raw("Waiting for a job from the pool."));
            if let Some(best) = &snapshot.best {
                lines.push(Line::default());
                lines.extend(self.best_hash(best, width));
            }
            self.append_hash_activity(&mut lines, snapshot, now, width);
            append_events(&mut lines, snapshot, now);
            return lines;
        };
        lines.push(Line::raw(format!(
            "Job {} · {} sat to your address · {}",
            work.job.id,
            grouped_digits(&work.paid_sats.to_string()),
            if self.binary { "Binary" } else { "Hex" }
        )));
        lines.push(nonce_scan(snapshot.scan_tested, &work.extranonce2, width));
        self.append_share_and_best(&mut lines, snapshot, work, width);
        self.append_hash_activity(&mut lines, snapshot, now, width);
        lines.push(Line::default());
        let last = snapshot.last.as_ref().map(|attempt| &attempt.hash);
        comparison(
            &mut lines,
            last,
            &work.job.network_target,
            self.binary,
            width,
        );
        if let Some(last) = snapshot.last {
            lines.push(Line::styled(
                format!("Current nonce: {} · 0x{:08x}", last.nonce(), last.nonce()),
                Style::new().fg(DIM),
            ));
        }
        append_events(&mut lines, snapshot, now);
        lines
    }

    fn append_share_and_best(
        &self,
        lines: &mut Lines,
        snapshot: &Snapshot,
        work: &Work,
        width: u16,
    ) {
        let split = hash_pair_split(width, self.binary);
        let column = split.unwrap_or(width);
        let mut share = vec![label("Share target")];
        if let Some(target) = &work.job.share_target {
            share.push(Line::raw(format!(
                "Difficulty {}",
                work.job.share_difficulty.as_deref().unwrap_or("—")
            )));
            share.extend(hash_lines(target, self.binary, column, None));
        } else {
            share.push(Line::styled(
                "Waiting for share difficulty",
                Style::new().fg(DIM),
            ));
        }
        let best = snapshot.best.as_ref().map_or_else(
            || {
                vec![
                    label("Best recorded hash"),
                    Line::styled("Waiting for a record", Style::new().fg(DIM)),
                ]
            },
            |best| self.best_hash(best, column),
        );
        lines.push(Line::default());
        append_sections(lines, share, best, width, split);
    }

    fn append_hash_activity(
        &self,
        lines: &mut Lines,
        snapshot: &Snapshot,
        now: Instant,
        width: u16,
    ) {
        let series = snapshot.history.series(now);
        let recent = series[HISTORY_SECONDS - RECENT_SECONDS..]
            .iter()
            .flatten()
            .min();
        let split = mine_column_split(width, self.binary);
        let sample_width = split.unwrap_or(width);
        let chart_width = split.map_or(width, |left| width - left - SECTION_GAP);
        let title = format!("Best sample in last {RECENT_SECONDS}s");
        let sample = recent.map_or_else(
            || {
                vec![
                    label(title.clone()),
                    Line::styled("Waiting for a hash", Style::new().fg(DIM)),
                ]
            },
            |hash| hash_section(&title, hash, self.binary, sample_width),
        );
        lines.push(Line::default());
        append_sections(
            lines,
            sample,
            hash_quality(series, chart_width),
            width,
            split,
        );
    }

    fn device(&self, snapshot: &Snapshot) -> Lines {
        let device_line = |text, backend| {
            Line::styled(
                text,
                if snapshot.backend == backend {
                    Style::new().fg(MINT).bold()
                } else {
                    Style::new()
                },
            )
        };
        let mut lines = vec![
            label("Mining device"),
            device_line(
                format!("0  CPU · {} threads", snapshot.threads),
                Backend::Cpu,
            ),
        ];
        match snapshot.gpu.adapters.as_deref() {
            None => lines.push(Line::raw("Detecting Vulkan GPUs…")),
            Some([]) => lines.push(Line::raw(
                "No compatible Vulkan GPU detected. CPU mining is available.",
            )),
            Some(adapters) => {
                lines.extend(adapters.iter().enumerate().map(|(index, name)| {
                    device_line(
                        format!("{}  {name} · Vulkan", index + 1),
                        Backend::Vulkan(index),
                    )
                }));
            }
        }
        lines.push(Line::raw(
            "Press G and enter a device number. Space starts or pauses mining.",
        ));
        lines.push(Line::raw(format!(
            "GPU intensity: {}% · Press I to change",
            snapshot.gpu.intensity.percent()
        )));
        match &snapshot.gpu.status {
            GpuStatus::Idle => {}
            GpuStatus::Preparing => {
                lines.push(Line::raw("Initializing the Vulkan compute shader…"))
            }
            GpuStatus::Ready => {
                lines.push(label(if snapshot.running {
                    "MINING ON GPU"
                } else {
                    "GPU READY · MINING PAUSED"
                }));
                lines.push(Line::raw(format!(
                    "{} H/s · {} total attempts",
                    snapshot.rate, snapshot.total
                )));
            }
            GpuStatus::Failed(error) => lines.push(Line::styled(
                format!("Vulkan error: {error}"),
                Style::new().fg(RED),
            )),
        }
        if matches!(snapshot.backend, Backend::Vulkan(_))
            && let Some(last) = snapshot.last
        {
            lines.push(label(format!(
                "Current hash · nonce 0x{:08x}",
                last.nonce()
            )));
            lines.extend(value_lines(&last.hash, self.binary, None));
        }
        lines
    }

    fn header(&self, snapshot: &Snapshot) -> Lines {
        let Some(work) = &snapshot.work else {
            return waiting();
        };
        let header = &snapshot
            .last
            .as_ref()
            .map_or_else(|| work.header(), |attempt| attempt.header);
        let mut lines = vec![
            label("The 80 bytes that go into SHA-256"),
            Line::raw("Offsets are decimal. Values appear in serialized header byte order."),
            Line::raw(
                "The miner reuses the SHA-256 state after the first 64 bytes for each nonce.",
            ),
        ];
        for (start, end, name, explanation) in [
            (
                0,
                4,
                "Version",
                format!("0x{:08x} · rules and signaling", work.job.version),
            ),
            (
                4,
                36,
                "Previous block hash",
                "Links the candidate to the previous block.".into(),
            ),
            (
                36,
                68,
                "Merkle root",
                "Commits to the coinbase and the remaining transactions in order.".into(),
            ),
            (
                68,
                72,
                "nTime",
                format!("{} · seconds since the Unix epoch", work.job.time),
            ),
            (
                72,
                76,
                "nBits",
                format!(
                    "0x{:08x} · compact network target · difficulty {}",
                    work.job.bits,
                    difficulty_text(&work.job.network_target)
                ),
            ),
            (
                76,
                80,
                "Nonce",
                format!(
                    "{} · 32-bit counter",
                    snapshot.last.as_ref().map_or(0, |a| a.nonce())
                ),
            ),
        ] {
            lines.push(label(format!("{start:02}–{:02}  {name}", end - 1)));
            lines.push(Line::raw(explanation));
            lines.extend(value_lines(&header[start..end], self.binary, None));
        }
        lines.push(label("Previous block hash in explorer byte order"));
        let mut previous = header[4..36].to_vec();
        previous.reverse();
        lines.extend(value_lines(&previous, self.binary, None));
        lines.push(label("Full header · serialized byte order"));
        if self.binary {
            lines.extend(value_lines(header, true, None));
        } else {
            bytes(&mut lines, header, 0);
        }
        if let Some(attempt) = snapshot.last {
            lines.push(label("1. SHA-256(header)"));
            lines.extend(value_lines(&attempt.first, self.binary, None));
            let mut second = attempt.hash;
            second.reverse();
            lines.push(label("2. SHA-256(first digest) · digest byte order"));
            lines.extend(value_lines(&second, self.binary, None));
            lines.push(label(
                "3. Reversed digest bytes · Bitcoin hash for target comparison",
            ));
            lines.extend(value_lines(&attempt.hash, self.binary, None));
        }
        lines
    }

    fn coinbase(&self, snapshot: &Snapshot) -> Lines {
        let Some(work) = &snapshot.work else {
            return waiting();
        };
        let mut lines = vec![
            label("Transaction #0 · creates the subsidy and collects the block fees"),
            Line::raw("Your address corresponds to the scriptPubKey in a payout output."),
        ];
        let mut offset = 0;
        for (name, part) in [
            (
                "coinb1 · first bytes from the pool",
                &work.job.coinbase_prefix,
            ),
            (
                "extranonce1 · bytes assigned to this connection",
                &work.extranonce1,
            ),
            ("extranonce2 · local counter", &work.extranonce2),
            (
                "coinb2 · final bytes from the pool",
                &work.job.coinbase_suffix,
            ),
        ] {
            lines.push(label(format!(
                "{name} · offset {offset}, {} bytes",
                part.len()
            )));
            bytes(&mut lines, part, offset);
            offset += part.len();
        }
        if let Some(height) = work.coinbase.input[0]
            .script_sig
            .instructions()
            .next()
            .and_then(Result::ok)
            .and_then(|first| first.script_num())
        {
            lines.push(label(format!(
                "BIP34 block height encoded in scriptSig: {height}"
            )));
        }
        lines.extend(transaction(
            &work.coinbase,
            Some((&work.payout_script, work.payout_offset())),
        ));
        lines
    }

    fn merkle(&self, snapshot: &Snapshot) -> Lines {
        let Some(work) = &snapshot.work else {
            return waiting();
        };
        let mut lines = vec![
            label("Coinbase → Merkle branch → header"),
            Line::raw(
                "Leaf 0 is the coinbase TXID. Each step double-hashes the current hash followed by its sibling.",
            ),
            Line::raw(
                "Hashes are shown in the byte order used for hashing. An unpaired node is duplicated.",
            ),
            label("Leaf 0 · coinbase digest without witness data"),
        ];
        lines.extend(value_lines(&work.path[0], self.binary, None));
        for (index, branch) in work.job.branches.iter().enumerate() {
            lines.push(label(format!(
                "Level {index} · sibling supplied by the pool"
            )));
            lines.extend(value_lines(branch, self.binary, None));
            lines.push(label("Result"));
            lines.extend(value_lines(&work.path[index + 1], self.binary, None));
        }
        lines.push(label("Final root · bytes written to header[36..68]"));
        lines.extend(value_lines(work.path.last().unwrap(), self.binary, None));
        lines
    }

    fn transactions(&self, snapshot: &Snapshot, height: u16) -> Lines {
        if let Some(lines) = &self.tx_detail {
            return lines.clone();
        }
        let Some(work) = &snapshot.work else {
            return waiting();
        };
        let mut lines = vec![
            label("Block candidate body · after the 80-byte header"),
            Line::raw(
                "CompactSize transaction count, then the coinbase and the remaining transactions in order.",
            ),
            Line::raw(snapshot.tx_status.clone()),
            Line::raw(format!("#0 coinbase · {}", work.coinbase.compute_txid())),
        ];
        if let Some(ids) = &snapshot.txids {
            let count = serialize(&bitcoin::VarInt((ids.len() + 1) as u64));
            lines.push(Line::raw(format!(
                "CompactSize at byte 80: {}",
                hex::encode(count)
            )));
            let rows = usize::from(height.saturating_sub(6)).max(1);
            let first = (self.tx_index / rows) * rows;
            for (index, txid) in ids.iter().enumerate().skip(first).take(rows) {
                let selected = index == self.tx_index;
                lines.push(Line::styled(
                    format!("{} #{} {txid}", if selected { "▸" } else { " " }, index + 1),
                    if selected {
                        Style::new().fg(GOLD).bold()
                    } else {
                        Style::default()
                    },
                ));
            }
        } else {
            lines.push(Line::raw(
                "The Merkle branch is enough to mine even when the pool does not return TXIDs.",
            ));
        }
        lines
    }

    fn best(&self, snapshot: &Snapshot, width: u16) -> Lines {
        let Some(best) = &snapshot.best else {
            return vec![Line::raw("No attempts recorded yet.")];
        };
        let nonce = u32::from_le_bytes(best.header[76..80].try_into().unwrap());
        let mut lines = self.best_hash(best, width);
        lines.extend([
            Line::raw(format!(
                "Job {} · nonce {nonce} (0x{nonce:08x}) · extranonce2 {}",
                best.job, best.nonce2
            )),
            label("Header that produced this hash"),
        ]);
        bytes(&mut lines, &best.header, 0);
        lines.push(Line::raw(format!(
            "Saved in {FILE_NAME} next to the executable."
        )));
        lines
    }

    fn best_hash(&self, best: &Best, width: u16) -> Lines {
        let mut lines = hash_section("Best recorded hash", &best.hash, self.binary, width);
        let timestamp = match best.recorded_at {
            Some(timestamp) => {
                let local = UtcOffset::local_offset_at(timestamp)
                    .ok()
                    .and_then(|offset| timestamp.checked_to_offset(offset))
                    .unwrap_or(timestamp);
                let (hour, minute, second) = local.to_hms();
                let offset = local.offset();
                let suffix = if offset.is_utc() {
                    String::new()
                } else {
                    let offset = offset.to_string();
                    offset.strip_suffix(":00").unwrap_or(&offset).to_owned()
                };
                format!(
                    "{} {hour:02}:{minute:02}:{second:02} UTC{suffix}",
                    local.date()
                )
            }
            None => "unknown (older record)".into(),
        };
        let caption = "Recorded at: ";
        if caption.len() + timestamp.len() <= usize::from(width) {
            lines.push(Line::styled(
                format!("{caption}{timestamp}"),
                Style::new().fg(DIM),
            ));
        } else {
            lines.push(Line::styled(caption.trim_end(), Style::new().fg(DIM)));
            lines.push(Line::styled(timestamp, Style::new().fg(DIM)));
        }
        lines
    }
}

fn fetch_transaction(txid: Txid) -> Result<Transaction> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(12)))
        .build()
        .into();
    let raw = agent
        .get(format!("https://blockstream.info/api/tx/{txid}/raw"))
        .call()
        .context("transaction unavailable in the explorer")?
        .body_mut()
        .with_config()
        .limit(4 * 1024 * 1024)
        .read_to_vec()?;
    let tx: Transaction = deserialize(&raw)?;
    ensure!(
        tx.compute_txid() == txid,
        "The response does not match the job’s TXID"
    );
    Ok(tx)
}

fn append_input(input: &mut String, text: &str, limit: usize) {
    input.extend(
        text.trim()
            .chars()
            .filter(|c| !c.is_control())
            .take(limit.saturating_sub(input.chars().count())),
    );
}

fn waiting() -> Lines {
    vec![Line::raw("Waiting for a job from the pool.")]
}

fn panel() -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BORDER))
        .style(Style::new().fg(TEXT).bg(SURFACE))
}

fn nonce_scan(tested: u64, extranonce: &[u8], width: u16) -> Line<'static> {
    let tested = tested.min(NONCE_SPACE);
    let percent = tested * 10_000 / NONCE_SPACE;
    let columns = if width >= 75 { 20 } else { 10 };
    let filled = (tested * columns as u64 / NONCE_SPACE) as usize;
    let extranonce = if extranonce.is_empty() {
        "—".into()
    } else {
        hex::encode(extranonce)
    };
    Line::from(vec![
        Span::styled("Nonce scan  ", Style::new().fg(TEXT).bold()),
        Span::styled("█".repeat(filled), Style::new().fg(MINT)),
        Span::styled("░".repeat(columns - filled), Style::new().fg(BORDER)),
        Span::raw(format!(
            "  {}.{:02}% · extranonce2 {extranonce}",
            percent / 100,
            percent % 100
        )),
    ])
}

fn min_hash_width(binary: bool) -> u16 {
    if binary { 71 } else { 32 }
}

fn mine_column_split(width: u16, binary: bool) -> Option<u16> {
    let available = width.saturating_sub(SECTION_GAP);
    let full_hash_width = if binary { 71 } else { 64 };
    let left = (available / 2).clamp(min_hash_width(binary), full_hash_width);
    (available >= left + CHART_MIN_WIDTH).then_some(left)
}

fn hash_pair_split(width: u16, binary: bool) -> Option<u16> {
    mine_column_split(width, binary)
        .filter(|&left| width - left - SECTION_GAP >= min_hash_width(binary))
}

fn hash_lines(
    hash: &Hash256,
    binary: bool,
    width: u16,
    highlight: Option<(usize, Style)>,
) -> Lines {
    let columns = if binary {
        8
    } else if (32..64).contains(&width) {
        16
    } else {
        32
    };
    value_rows(hash, binary, columns, highlight).collect()
}

fn hash_section(title: &str, hash: &Hash256, binary: bool, width: u16) -> Lines {
    let mut lines = vec![label(title.to_owned())];
    let difficulty = format!("Difficulty {}", difficulty_text(hash));
    let zeros = format!("{} leading zero bits", mining::leading_zero_bits(hash));
    if difficulty.len() + 3 + zeros.len() <= usize::from(width) {
        lines.push(Line::raw(format!("{difficulty} · {zeros}")));
    } else {
        lines.extend([Line::raw(difficulty), Line::raw(zeros)]);
    }
    lines.extend(hash_lines(hash, binary, width, None));
    lines
}

fn append_sections(lines: &mut Lines, left: Lines, right: Lines, width: u16, split: Option<u16>) {
    let Some(split) = split.filter(|&split| {
        left.iter().all(|line| line.width() <= usize::from(split))
            && right
                .iter()
                .all(|line| line.width() <= usize::from(width - split - SECTION_GAP))
    }) else {
        lines.extend(left);
        lines.push(Line::default());
        lines.extend(right);
        return;
    };

    let rows = left.len().max(right.len());
    let mut left = left.into_iter();
    let mut right = right.into_iter();
    for _ in 0..rows {
        let mut line = left.next().unwrap_or_default();
        line.spans
            .push(Span::raw(" ".repeat(usize::from(split) - line.width())));
        line.spans
            .push(Span::styled(" │  ", Style::new().fg(BORDER)));
        if let Some(next) = right.next() {
            line.spans.extend(next.spans);
        }
        lines.push(line);
    }
}

fn hash_quality(series: [Option<Hash256>; HISTORY_SECONDS], width: u16) -> Lines {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let spacing = if width >= CHART_AXIS + HISTORY_SECONDS as u16 * 2 {
        " "
    } else {
        ""
    };
    let zeros = series.map(|hash| hash.map(|hash| mining::leading_zero_bits(&hash)));
    let axis = Style::new().fg(DIM);
    let mut lines = vec![label(format!(
        "Hash quality · {HISTORY_SECONDS}s · zero bits"
    ))];

    for row in (0..CHART_ROWS).rev() {
        let base = row * CHART_ROW_BITS;
        let mut spans = vec![Span::styled(
            format!("{:>2} │", base + CHART_ROW_BITS),
            axis,
        )];
        spans.extend(zeros.iter().enumerate().map(|(index, &bits)| {
            let segment = bits
                .unwrap_or_default()
                .saturating_sub(base)
                .min(CHART_ROW_BITS);
            let symbol = match bits {
                Some(value) if value > CHART_MAX_BITS && row == CHART_ROWS - 1 => '↑',
                Some(_) if segment > 0 => BARS[usize::from(segment - 1)],
                Some(0) if row == 0 => '•',
                None if row == 0 => '·',
                _ => ' ',
            };
            let color = if bits.is_none() {
                BORDER
            } else if index == HISTORY_SECONDS - 1 {
                MINT
            } else {
                GOLD
            };
            Span::styled(format!("{symbol}{spacing}"), Style::new().fg(color))
        }));
        lines.push(Line::from(spans));
    }

    let plot_width = HISTORY_SECONDS * (1 + spacing.len());
    lines.push(Line::styled(
        format!(" 0 └{} now", "─".repeat(plot_width - 4)),
        axis,
    ));
    lines
}

fn append_events(lines: &mut Lines, snapshot: &Snapshot, now: Instant) {
    if snapshot.events.is_empty() {
        return;
    }
    lines.push(Line::default());
    lines.push(label("Recent events"));
    for event in snapshot.events.iter().rev() {
        let seconds = now.saturating_duration_since(event.at).as_secs();
        let age = match seconds {
            0 => "now".into(),
            1..60 => format!("{seconds}s ago"),
            _ => format!("{}m ago", seconds / 60),
        };
        let count = if event.count > 1 {
            format!(" ×{}", event.count)
        } else {
            String::new()
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{age:>8}  "), Style::new().fg(DIM)),
            Span::raw(format!("{}{count}", event.text)),
        ]));
    }
}

fn draw_metrics(frame: &mut Frame, area: Rect, snapshot: &Snapshot) {
    let areas = Layout::horizontal([Constraint::Fill(1); 4])
        .spacing(1)
        .areas::<4>(area);
    for (area, name, values) in [
        (
            areas[0],
            "Hashrate",
            vec![
                Line::styled(hashrate_text(snapshot.rate), Style::new().fg(GOLD).bold()),
                if snapshot.rate >= 1_000 {
                    Line::styled(
                        format!("{} H/s", grouped_digits(&snapshot.rate.to_string())),
                        Style::new().fg(DIM),
                    )
                } else {
                    Line::default()
                },
            ],
        ),
        (
            areas[1],
            "Attempts",
            vec![Line::styled(
                grouped_digits(&snapshot.total.to_string()),
                Style::new().bold(),
            )],
        ),
        (
            areas[2],
            "Shares",
            vec![
                Line::styled(
                    format!(
                        "{} accepted",
                        grouped_digits(&snapshot.accepted.to_string())
                    ),
                    Style::new().fg(MINT).bold(),
                ),
                Line::styled(
                    format!(
                        "{} rejected",
                        grouped_digits(&snapshot.rejected.to_string())
                    ),
                    Style::new().fg(if snapshot.rejected == 0 { DIM } else { RED }),
                ),
            ],
        ),
        (
            areas[3],
            "Block candidates",
            vec![Line::styled(
                grouped_digits(&snapshot.network_hits.to_string()),
                Style::new().bold(),
            )],
        ),
    ] {
        let card = panel()
            .style(Style::new().bg(RAISED))
            .padding(Padding::horizontal(1))
            .title(Line::styled(format!(" {name} "), Style::new().fg(DIM)));
        frame.render_widget(
            Paragraph::new(values).block(card),
            Rect { height: 4, ..area },
        );
    }
}

fn label(text: impl Into<String>) -> Line<'static> {
    Line::styled(
        text.into(),
        Style::new().fg(GOLD).add_modifier(Modifier::BOLD),
    )
}

fn difficulty_text(hash: &Hash256) -> String {
    let difficulty = mining::hash_difficulty(hash);
    if difficulty.is_infinite() {
        "∞".into()
    } else if (0.0001..1e16).contains(&difficulty) {
        let precision = if difficulty >= 1_000_000.0 { 2 } else { 6 };
        let fixed = format!("{difficulty:.precision$}");
        let (whole, fraction) = fixed.split_once('.').unwrap();
        let mut text = grouped_digits(whole);
        let fraction = fraction.trim_end_matches('0');
        if !fraction.is_empty() {
            text.push('.');
            text.push_str(fraction);
        }
        text
    } else {
        format!("{difficulty:.4e}")
    }
}

fn grouped_digits(digits: &str) -> String {
    let mut text = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.bytes().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            text.push(',');
        }
        text.push(char::from(digit));
    }
    text
}

fn hashrate_text(rate: u64) -> String {
    for (unit, divisor) in [
        ("TH/s", 1_000_000_000_000),
        ("GH/s", 1_000_000_000),
        ("MH/s", 1_000_000),
        ("kH/s", 1_000),
    ] {
        if rate >= divisor {
            return format!("{:.2} {unit}", rate as f64 / divisor as f64);
        }
    }
    format!("{rate} H/s")
}

fn value_lines(
    bytes: &[u8],
    binary: bool,
    highlight: Option<(usize, Style)>,
) -> impl Iterator<Item = Line<'static>> {
    let columns = if binary { 8 } else { bytes.len().max(1) };
    value_rows(bytes, binary, columns, highlight)
}

fn value_rows(
    bytes: &[u8],
    binary: bool,
    columns: usize,
    highlight: Option<(usize, Style)>,
) -> impl Iterator<Item = Line<'static>> {
    bytes.chunks(columns).enumerate().map(move |(row, chunk)| {
        Line::from_iter(chunk.iter().enumerate().map(|(column, byte)| {
            let style = match highlight {
                Some((index, style)) if index == row * columns + column => style,
                _ => Style::default(),
            };
            let text = if binary {
                let separator = if column == 0 { "" } else { " " };
                format!("{separator}{byte:08b}")
            } else {
                format!("{byte:02x}")
            };
            Span::styled(text, style)
        }))
    })
}

fn bytes(lines: &mut Lines, bytes: &[u8], offset: usize) {
    for (index, chunk) in bytes.chunks(12).enumerate() {
        lines.push(Line::raw(format!(
            "{:05}  {}",
            offset + index * 12,
            hex::encode(chunk)
        )));
    }
}

fn comparison(
    lines: &mut Lines,
    hash: Option<&Hash256>,
    target: &Hash256,
    binary: bool,
    width: u16,
) {
    let mut highlight = None;
    let state = match hash {
        Some(hash) => {
            let meets_target = hash <= target;
            let color = if meets_target { MINT } else { DIM };
            highlight = hash
                .iter()
                .zip(target)
                .position(|(a, b)| a != b)
                .map(|index| {
                    (
                        index,
                        Style::new()
                            .fg(if meets_target { MINT } else { GOLD })
                            .bold()
                            .underlined(),
                    )
                });
            Span::styled(
                if meets_target {
                    "Hash ≤ network target"
                } else {
                    "Hash > network target"
                },
                Style::new().fg(color),
            )
        }
        None => Span::styled("Waiting for an attempt", Style::new().fg(DIM)),
    };
    lines.push(Line::from(vec![
        Span::styled("Proof of work", Style::new().fg(TEXT).bold()),
        Span::styled("  ·  ", Style::new().fg(DIM)),
        state,
    ]));
    let split = hash_pair_split(width, binary);
    let column = split.unwrap_or(width);
    let mut target_lines = vec![
        label("Network target"),
        Line::raw(format!("Difficulty {}", difficulty_text(target))),
    ];
    target_lines.extend(hash_lines(target, binary, column, highlight));
    if hash.is_none() && split.is_none() {
        lines.extend(target_lines);
        return;
    }
    let mut current = vec![label("Current hash")];
    if let Some(hash) = hash {
        current.push(Line::raw(format!(
            "{} leading zero bits",
            mining::leading_zero_bits(hash)
        )));
        current.extend(hash_lines(hash, binary, column, highlight));
    }
    append_sections(lines, target_lines, current, width, split);
}

fn transaction(tx: &Transaction, payout: Option<(&Script, usize)>) -> Lines {
    let mut lines = vec![
        label("Transaction fields"),
        Line::raw(format!("TXID {}", tx.compute_txid())),
        Line::raw(format!(
            "Version {} · locktime {} · {} input(s) · {} output(s)",
            tx.version.0,
            tx.lock_time.to_consensus_u32(),
            tx.input.len(),
            tx.output.len()
        )),
    ];
    for (index, input) in tx.input.iter().enumerate() {
        lines.push(label(format!("Input #{index}")));
        lines.push(Line::raw(format!(
            "Outpoint {} · sequence {:08x}",
            input.previous_output, input.sequence.0
        )));
        lines.push(Line::raw(format!(
            "scriptSig {}",
            hex::encode(input.script_sig.as_bytes())
        )));
        for (index, item) in input.witness.iter().enumerate() {
            lines.push(Line::raw(format!(
                "Witness #{index} · {} bytes",
                item.len()
            )));
            bytes(&mut lines, item, 0);
        }
    }
    let mut offset = payout.map_or(0, |(_, offset)| offset);
    for (index, output) in tx.output.iter().enumerate() {
        let yours = payout.is_some_and(|(script, _)| script == output.script_pubkey.as_script());
        let encoded_len = serialize(output).len();
        let script = output.script_pubkey.as_bytes();
        let location = if payout.is_some() && !script.is_empty() {
            format!(
                " · script at coinbase bytes {}–{}",
                offset + encoded_len - script.len(),
                offset + encoded_len - 1
            )
        } else {
            String::new()
        };
        lines.push(Line::styled(
            format!(
                "Output #{index} · {} sat{}{location}",
                output.value.to_sat(),
                if yours { " · YOUR PAYOUT" } else { "" }
            ),
            Style::new().fg(if yours { MINT } else { GOLD }).bold(),
        ));
        if let Ok(address) = Address::from_script(&output.script_pubkey, Network::Bitcoin) {
            lines.push(Line::raw(format!("Address {address}")));
        }
        if script.starts_with(&[0x6a, 0x24, 0xaa, 0x21, 0xa9, 0xed]) {
            lines.push(Line::raw(
                "OP_RETURN · SegWit commitment (transaction witness data)",
            ));
        }
        lines.push(Line::raw(format!("scriptPubKey {}", hex::encode(script))));
        offset += encoded_len;
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{CompactTarget, Target};

    #[test]
    fn mining_activity_displays_scan_progress_and_leading_zero_bits() {
        let scan = nonce_scan(NONCE_SPACE / 2, &[1, 2], 100).to_string();
        assert!(scan.contains("50.00% · extranonce2 0102"));
        assert_eq!(scan.matches('█').count(), 10);

        let mut sample = [u8::MAX; 32];
        sample[..2].copy_from_slice(&[0, 0x10]);
        assert_eq!(mining::leading_zero_bits(&sample), 11);
        assert_eq!(mining::leading_zero_bits(&[0; 32]), 256);
    }

    #[test]
    fn hash_quality_uses_the_same_scale_when_a_peak_leaves_the_window() {
        let mut eleven_bits = [u8::MAX; 32];
        eleven_bits[..2].copy_from_slice(&[0, 0x10]);
        let mut series = [None; HISTORY_SECONDS];
        series[0] = Some([0; 32]);
        series[HISTORY_SECONDS - 1] = Some(eleven_bits);
        let with_peak = hash_quality(series, CHART_MIN_WIDTH);
        series[0] = None;
        let without_peak = hash_quality(series, CHART_MIN_WIDTH);

        assert!(with_peak[1].to_string().starts_with("64 │↑"));
        assert!(with_peak.last().unwrap().to_string().starts_with(" 0 └"));
        assert_eq!(with_peak.last(), without_peak.last());
        for (before, after) in with_peak[1..].iter().zip(&without_peak[1..]) {
            assert_eq!(before.spans.last(), after.spans.last());
        }
        let bottom = usize::from(CHART_ROWS);
        assert!(without_peak[bottom - 1].to_string().ends_with('▃'));
        assert!(without_peak[bottom].to_string().ends_with('█'));
        assert!(
            without_peak
                .iter()
                .all(|line| line.width() <= usize::from(CHART_MIN_WIDTH))
        );
    }

    #[test]
    fn network_difficulty_and_binary_rows_fit_both_comparison_layouts() {
        let target = Target::from_compact(CompactTarget::from_consensus(0x1c00ffff)).to_be_bytes();
        let hash = [0x55; 32];
        for (width, expected_rows) in [(100, 8), (160, 4)] {
            let mut lines = Vec::new();
            comparison(&mut lines, Some(&hash), &target, true, width);
            assert!(lines[1].to_string().starts_with("Network target"));
            assert!(lines[2].to_string().starts_with("Difficulty 256"));
            let rows: Vec<_> = lines
                .iter()
                .map(ToString::to_string)
                .filter(|row| {
                    row.as_bytes()
                        .get(..8)
                        .is_some_and(|prefix| prefix.iter().all(|byte| matches!(byte, b'0' | b'1')))
                })
                .collect();
            assert_eq!(rows.len(), expected_rows);
            for row in rows {
                let bytes: Vec<_> = row.split_whitespace().filter(|part| *part != "│").collect();
                assert_eq!(bytes.len(), if width >= 146 { 16 } else { 8 });
                assert!(bytes.iter().all(|byte| byte.len() == 8));
            }
        }
    }

    #[test]
    fn comparison_places_its_divider_on_the_shared_dashboard_column() {
        let hash = [0x55; 32];
        let target = [0x33; 32];
        for (width, binary) in [(74, false), (114, false), (154, true), (194, true)] {
            let split = mine_column_split(width, binary).unwrap();
            assert_eq!(hash_pair_split(width, binary), Some(split));
            let mut lines = Vec::new();
            comparison(&mut lines, Some(&hash), &target, binary, width);
            assert!(lines[1].to_string().ends_with("Current hash"));
            assert!(lines[1..].iter().all(|line| {
                line.to_string().chars().position(|symbol| symbol == '│')
                    == Some(usize::from(split + 1))
            }));
        }
    }
}
