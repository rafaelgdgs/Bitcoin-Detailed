use std::sync::{Arc, mpsc};
use std::time::Duration;

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
use ratatui::widgets::{Block, Paragraph, Tabs, Wrap};
use ratatui::{DefaultTerminal, Frame};
use time::UtcOffset;

use crate::engine::{self, Command, Config, Connection, Snapshot};
use crate::mining::{self, Hash256, Work};
use crate::storage::{Best, FILE_NAME, SavedState, SetupForm, Storage};

const GOLD: Color = Color::Rgb(255, 189, 91);
const MINT: Color = Color::Rgb(109, 224, 180);
const DIM: Color = Color::Rgb(145, 153, 169);
const TABS: [&str; 6] = [
    "1 Mine",
    "2 Header",
    "3 Coinbase",
    "4 Merkle",
    "5 Transactions",
    "6 Best",
];

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
}

impl Prompt {
    fn input_mut(&mut self) -> &mut String {
        match self {
            Self::Nonce(_, input) | Self::Threads(input) => input,
        }
    }
}

struct App {
    form: SetupForm,
    storage: Storage,
    initial_best: Option<Best>,
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
                        Prompt::Threads(_) => c.is_ascii_digit(),
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
                            let best = match self.storage.save_settings(&self.form) {
                                Ok(state) => {
                                    self.notice.clear();
                                    state.best
                                }
                                Err(error) => {
                                    self.notice = format!("Could not save {FILE_NAME}: {error:#}");
                                    self.initial_best.take()
                                }
                            };
                            self.connection =
                                Some(engine::start(config, self.storage.clone(), best));
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
            KeyCode::Char(c @ '1'..='6') => self.select_tab((c as u8 - b'1') as usize),
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
        let area = frame.area();
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
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(2),
            Constraint::Length(3),
        ])
        .areas(area);
        frame.render_widget(
            Line::from(vec![
                Span::styled(
                    " BITCOIN DETAILED ",
                    Style::new().fg(Color::Black).bg(GOLD).bold(),
                ),
                Span::styled("  Bitcoin mainnet / Stratum V1 TCP", Style::new().fg(DIM)),
            ]),
            title,
        );
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(format!(" {}", snapshot.status), Style::new().fg(MINT)),
                Line::styled(format!(" {}", self.form[0]), Style::new().fg(DIM)),
            ]),
            state,
        );
        frame.render_widget(
            Tabs::new(TABS)
                .select(self.tab)
                .highlight_style(Style::new().fg(GOLD).bold())
                .style(Style::new().fg(DIM))
                .divider(" │ "),
            tabs,
        );
        let border = Block::bordered()
            .title(format!(" {} ", TABS[self.tab].split_once(' ').unwrap().1))
            .border_style(Style::new().fg(DIM));
        let inner = border.inner(body);
        frame.render_widget(border, body);
        let content = match self.tab {
            0 => self.dashboard(snapshot),
            1 => self.header(snapshot),
            2 => self.coinbase(snapshot),
            3 => self.merkle(snapshot),
            4 => self.transactions(snapshot, inner.height),
            5 => self.best(snapshot),
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
            None => {
                " 1–6/Tab tabs · Space mine/pause · C threads · N nonce · B hex/bin · T TXIDs · Q quit".into()
            }
        };
        let message = if self.notice.is_empty() {
            &snapshot.message
        } else {
            &self.notice
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(prompt, Style::new().fg(GOLD)),
                Line::raw(format!(" {message}")),
            ])
            .wrap(Wrap { trim: false }),
            footer,
        );
    }

    fn draw_form(&self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![
            label("Bitcoin Detailed"),
            Line::raw("Solo CPU mining with live Bitcoin mainnet jobs from the pool."),
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
            lines.push(Line::styled(
                format!(
                    "{} {name}  › {value}{}",
                    if self.field == i { "▸" } else { " " },
                    if self.field == i { "▌" } else { "" }
                ),
                Style::new().fg(if self.field == i { GOLD } else { Color::Reset }),
            ));
        }
        lines.push(Line::raw("Tab/↑↓ select field · Enter connect · Esc quit"));
        if !self.notice.is_empty() {
            lines.push(Line::styled(
                self.notice.clone(),
                Style::new().fg(Color::LightRed),
            ));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .block(Block::bordered())
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn dashboard(&self, snapshot: &Snapshot) -> Lines {
        let mut lines = vec![
            label(format!(
                "{} · Threads: {}",
                if snapshot.running {
                    "MINING ON CPU"
                } else {
                    "MINING PAUSED"
                },
                snapshot.threads
            )),
            Line::raw(format!(
                "{} H/s · {} attempts · {} shares accepted / {} rejected · {} block candidates",
                snapshot.rate,
                snapshot.total,
                snapshot.accepted,
                snapshot.rejected,
                snapshot.network_hits
            )),
        ];
        if let Some(candidate) = &snapshot.candidate {
            lines.push(Line::styled(candidate.clone(), Style::new().fg(MINT)));
        }
        let Some(work) = &snapshot.work else {
            lines.push(Line::raw("Waiting for a job from the pool."));
            if let Some(best) = &snapshot.best {
                self.append_best_hash(&mut lines, best);
            }
            return lines;
        };
        lines.push(Line::raw(format!(
            "Job {} · {} sat to your address",
            work.job.id, work.paid_sats
        )));
        lines.push(Line::default());
        lines.push(label(
            "Proof of work · the hash must be at or below the target",
        ));
        let last = snapshot.last.as_ref().map(|attempt| &attempt.hash);
        comparison(&mut lines, last, &work.job.network_target, self.binary);
        if let Some(target) = &work.job.share_target {
            lines.push(label(format!(
                "Share target · difficulty {}",
                work.job.share_difficulty.as_deref().unwrap_or("—")
            )));
            lines.extend(value_lines(target, self.binary, None));
            lines.push(Line::raw(
                "Shares measure work for the pool. A solo block reward requires meeting the network target.",
            ));
        } else {
            lines.push(Line::raw(
                "The pool has not set a share difficulty for this job.",
            ));
        }
        if let Some(best) = &snapshot.best {
            self.append_best_hash(&mut lines, best);
        }
        if let Some(last) = snapshot.last {
            lines.push(Line::raw(format!(
                "Current nonce: {} · 0x{:08x}",
                last.nonce(),
                last.nonce()
            )));
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
                format!("0x{:08x} · network target in compact form", work.job.bits),
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

    fn best(&self, snapshot: &Snapshot) -> Lines {
        let Some(best) = &snapshot.best else {
            return vec![Line::raw("No attempts recorded yet.")];
        };
        let nonce = u32::from_le_bytes(best.header[76..80].try_into().unwrap());
        let mut lines = Vec::new();
        self.append_best_hash(&mut lines, best);
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

    fn append_best_hash(&self, lines: &mut Lines, best: &Best) {
        lines.push(label(format!(
            "Best recorded hash · difficulty {}",
            difficulty_text(&best.hash)
        )));
        lines.push(Line::raw(match best.recorded_at {
            Some(timestamp) => {
                let utc = timestamp.to_offset(UtcOffset::UTC);
                let (hour, minute, second) = utc.to_hms();
                format!(
                    "Recorded at: {} {hour:02}:{minute:02}:{second:02} UTC",
                    utc.date()
                )
            }
            None => "Recorded at: unknown (older record)".into(),
        }));
        lines.extend(value_lines(&best.hash, self.binary, None));
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
    } else if (0.0001..1_000_000.0).contains(&difficulty) {
        format!("{difficulty:.6}")
    } else {
        format!("{difficulty:.4e}")
    }
}

fn value_lines(
    bytes: &[u8],
    binary: bool,
    highlight: Option<(usize, Style)>,
) -> impl Iterator<Item = Line<'static>> {
    let columns = if binary { 8 } else { bytes.len().max(1) };
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

fn comparison(lines: &mut Lines, hash: Option<&[u8; 32]>, target: &[u8; 32], binary: bool) {
    if let Some(hash) = hash {
        let color = if hash <= target {
            MINT
        } else {
            Color::LightRed
        };
        lines.push(Line::styled(
            if hash <= target {
                "Hash ≤ network target"
            } else {
                "Hash > network target"
            },
            Style::new().fg(color),
        ));
        let difference = hash.iter().zip(target).position(|(a, b)| a != b);
        let highlight = difference.map(|index| (index, Style::new().fg(color).bold()));
        for (caption, value) in [("Current hash", hash), ("Network target", target)] {
            lines.push(Line::raw(caption));
            lines.extend(value_lines(value, binary, highlight));
        }
    } else {
        lines.push(Line::raw("Current hash: waiting for an attempt"));
        lines.push(Line::raw("Network target"));
        lines.extend(value_lines(target, binary, None));
    }
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
