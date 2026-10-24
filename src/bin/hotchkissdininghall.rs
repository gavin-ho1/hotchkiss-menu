//! hotchkissdininghall (aka dh, dininghall): TUI for the Hotchkiss dining hall menu.
//!
//! Draws instantly from the disk cache, then refreshes stale weeks in the
//! background. Two panes: the meal list (left) and the menu (right); h/l move
//! focus between them, j/k move/scroll within the focused one.

use chrono::{DateTime, Days, NaiveDate, NaiveTime};
use chrono_tz::Tz;
use hotchkiss_menu::*;
use ratatui::{
    DefaultTerminal, Frame,
    buffer::Buffer,
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Widget, Wrap,
    },
};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::Duration;

const ACCENT: Color = Color::Cyan;
const CARD_MAX_W: u16 = 64;
const CARD_MIN_W: u16 = 38;

struct Slot {
    date: NaiveDate,
    meal: Meal,
}

enum Loaded {
    Loading,
    Empty,
    Food,
}

#[derive(PartialEq)]
enum Focus {
    List,
    Menu,
}

struct App {
    cfg: Arc<Config>,
    today: NaiveDate,
    slots: Vec<Slot>,
    sel: usize,
    user_moved: bool,
    menus: Menus,
    keys: Vec<WeekKey>,
    focus: Focus,
    show_all: bool,
    details: bool,
    scroll: u16,
    view_h: u16,
    pending: usize,
    errors: Vec<String>,
    show_config: bool,
    quit: bool,
    tx: mpsc::Sender<(WeekKey, Result<Week, String>)>,
    rx: mpsc::Receiver<(WeekKey, Result<Week, String>)>,
}

fn main() {
    let path = match std::env::args_os().nth(1) {
        Some(a) if a == "-h" || a == "--help" => {
            println!("usage: hotchkissdininghall [CONFIG]   (default: ~/.config/hotchkiss-menu/config.toml)");
            return;
        }
        Some(a) => Ok(PathBuf::from(a)),
        None => default_config_path(),
    };
    let cfg = match path.and_then(|p| Config::load(&p)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            std::process::exit(1);
        }
    };
    // Printed to the normal screen, so it's still visible after quitting.
    eprintln!("{}", cfg.describe());

    let mut app = App::new(Arc::new(cfg));
    let mut terminal = ratatui::init();
    let res = app.run(&mut terminal);
    ratatui::restore();
    if let Err(e) = res {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

impl App {
    fn new(cfg: Arc<Config>) -> App {
        let today = cfg.now().date_naive();
        let dates: Vec<NaiveDate> = (0..=cfg.lookahead_days as u64).map(|i| today + Days::new(i)).collect();
        let slots = dates
            .iter()
            .flat_map(|&date| cfg.meals_on(date).iter().map(move |m| Slot { date, meal: m.clone() }))
            .collect();
        let keys = keys_for(dates.iter().copied(), &cfg.all_menu_types());

        // Everything cached is shown immediately; stale or missing weeks are refetched.
        let mut menus = Menus::default();
        let mut stale = Vec::new();
        for k in &keys {
            match read_cache(&cfg, k) {
                Some((w, age)) => {
                    menus.insert(k.clone(), w);
                    if age >= cfg.cache_ttl {
                        stale.push(k.clone());
                    }
                }
                None => stale.push(k.clone()),
            }
        }

        let (tx, rx) = mpsc::channel();
        let mut app = App {
            cfg, today, slots, sel: 0, user_moved: false, menus, keys, focus: Focus::List,
            show_all: false, details: true, scroll: 0, view_h: 0, pending: 0, errors: Vec::new(),
            show_config: false, quit: false, tx, rx,
        };
        app.sel = app.next_meal();
        app.fetch(stale);
        app
    }

    fn fetch(&mut self, keys: Vec<WeekKey>) {
        self.pending += keys.len();
        for k in keys {
            let (cfg, tx) = (self.cfg.clone(), self.tx.clone());
            std::thread::spawn(move || {
                let res = hotchkiss_menu::fetch(&cfg, &k);
                let _ = tx.send((k, res));
            });
        }
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        while !self.quit {
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(250))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press {
                        self.key(k);
                    }
                }
            }
            while let Ok((k, res)) = self.rx.try_recv() {
                self.pending -= 1;
                match res {
                    Ok(w) => self.menus.insert(k, w),
                    Err(e) if !self.errors.contains(&e) => self.errors.push(e),
                    Err(_) => {}
                }
                if self.pending == 0 && !self.user_moved {
                    self.sel = self.next_meal();
                }
            }
        }
        Ok(())
    }

    fn state(&self, slot: &Slot) -> Loaded {
        match self.menus.meal(&slot.meal, slot.date) {
            None => Loaded::Loading,
            Some(s) if s.is_empty() => Loaded::Empty,
            Some(_) => Loaded::Food,
        }
    }

    /// The meal being served now, or the next one with a posted menu.
    fn next_meal(&self) -> usize {
        let now = self.cfg.now();
        self.slots
            .iter()
            .position(|s| self.cfg.at(s.date, s.meal.end) > now && !matches!(self.state(s), Loaded::Empty))
            .unwrap_or(0)
    }

    fn select(&mut self, i: usize) {
        if !self.slots.is_empty() {
            self.sel = i.min(self.slots.len() - 1);
            self.scroll = 0;
            self.user_moved = true;
        }
    }

    fn jump_day(&mut self, forward: bool) {
        let Some(cur) = self.slots.get(self.sel).map(|s| s.date) else { return };
        let target = if forward {
            self.slots.iter().position(|s| s.date > cur)
        } else {
            let prev = self.slots.iter().rev().find(|s| s.date < cur).map(|s| s.date);
            prev.and_then(|p| self.slots.iter().position(|s| s.date == p))
        };
        if let Some(i) = target {
            self.select(i);
        }
    }

    fn key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if self.show_config {
            if matches!(k.code, KeyCode::Esc | KeyCode::Char('c') | KeyCode::Char('q')) {
                self.show_config = false;
            }
            return;
        }
        let half = (self.view_h / 2).max(1);

        // Keys that depend on which pane has focus.
        let handled = match self.focus {
            Focus::List => match k.code {
                KeyCode::Down | KeyCode::Char('j') => { self.select(self.sel + 1); true }
                KeyCode::Up | KeyCode::Char('k') => { self.select(self.sel.saturating_sub(1)); true }
                KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => { self.focus = Focus::Menu; true }
                KeyCode::Esc => { self.quit = true; true }
                _ => false,
            },
            Focus::Menu => match k.code {
                KeyCode::Down | KeyCode::Char('j') => { self.scroll = self.scroll.saturating_add(1); true }
                KeyCode::Up | KeyCode::Char('k') => { self.scroll = self.scroll.saturating_sub(1); true }
                KeyCode::Char('d') if ctrl => { self.scroll = self.scroll.saturating_add(half); true }
                KeyCode::Char('u') if ctrl => { self.scroll = self.scroll.saturating_sub(half); true }
                KeyCode::Char('g') => { self.scroll = 0; true }
                KeyCode::Char('G') => { self.scroll = u16::MAX; true }
                KeyCode::Left | KeyCode::Char('h') | KeyCode::Esc => { self.focus = Focus::List; true }
                _ => false,
            },
        };
        if handled {
            return;
        }

        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char(']') | KeyCode::Tab => self.jump_day(true),
            KeyCode::Char('[') | KeyCode::BackTab => self.jump_day(false),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll = self.scroll.saturating_add(self.view_h.max(1)),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(self.view_h.max(1)),
            KeyCode::Char('n') | KeyCode::Home => {
                self.sel = self.next_meal();
                self.scroll = 0;
                self.user_moved = false;
            }
            KeyCode::Char('a') => {
                self.show_all = !self.show_all;
                self.scroll = 0;
            }
            KeyCode::Char('i') => self.details = !self.details,
            KeyCode::Char('r') if self.pending == 0 => {
                self.errors.clear();
                self.fetch(self.keys.clone());
            }
            KeyCode::Char('c') => self.show_config = true,
            _ => {}
        }
    }

    // ------------------------------------------------------------- drawing

    fn draw(&mut self, f: &mut Frame) {
        let [top, body, bottom] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1), Constraint::Length(1)]).areas(f.area());
        let [left, right] = Layout::horizontal([Constraint::Length(24), Constraint::Fill(1)]).areas(body);

        let now = self.cfg.now();
        f.render_widget(
            Line::from(vec![
                " Hotchkiss Dining Hall ".bold().fg(Color::Black).bg(ACCENT),
                format!("  {}", now.format("%A, %b %-d · %-I:%M %p")).dim(),
            ]),
            top,
        );
        let status = if self.pending > 0 {
            Span::from("updating… ").fg(Color::Yellow)
        } else if !self.errors.is_empty() {
            Span::from("offline: showing cached menus ").fg(Color::Red)
        } else {
            Span::raw("")
        };
        f.render_widget(Line::from(status).right_aligned(), top);

        self.draw_list(f, left, now);
        self.draw_menu(f, right, now);
        self.draw_help(f, bottom);
        if self.show_config {
            self.draw_config(f);
        }
    }

    fn draw_help(&self, f: &mut Frame, area: Rect) {
        let all = if self.show_all { "main only" } else { "all stations" };
        let info = if self.details { "hide details" } else { "details" };
        let keys: Vec<(&str, &str)> = match self.focus {
            Focus::List => vec![("j/k", "meal"), ("l", "open"), ("[ ]", "day"), ("n", "next meal"), ("a", all),
                                ("i", info), ("r", "refresh"), ("c", "config"), ("q", "quit")],
            Focus::Menu => vec![("j/k", "scroll"), ("^d/^u", "half page"), ("g/G", "top/bottom"), ("h", "back"),
                                ("a", all), ("i", info), ("q", "quit")],
        };
        let mut spans = vec![Span::raw(" ")];
        for (k, what) in keys {
            spans.push(Span::from(k).fg(ACCENT));
            spans.push(Span::from(format!(" {what}   ")).dim());
        }
        let legend = Line::from(vec!["VG".green().bold(), " vegan  ".dim(), "V".green(), " vegetarian ".dim()]);
        f.render_widget(Line::from(spans), area);
        f.render_widget(legend.right_aligned(), area);
    }

    fn day_label(&self, d: NaiveDate) -> Line<'static> {
        let date = d.format("%a %b %-d").to_string();
        match (d - self.today).num_days() {
            0 => Line::from(vec!["Today ".bold().fg(Color::Yellow), date.dim()]),
            1 => Line::from(vec!["Tomorrow ".bold(), date.dim()]),
            _ => Line::from(vec![d.format("%A ").to_string().bold(), d.format("%b %-d").to_string().dim()]),
        }
    }

    fn draw_list(&self, f: &mut Frame, area: Rect, now: DateTime<Tz>) {
        let focused = self.focus == Focus::List;
        let mut items = Vec::new();
        let mut sel_row = 0;
        for (i, slot) in self.slots.iter().enumerate() {
            if i == 0 || slot.date != self.slots[i - 1].date {
                if i > 0 {
                    items.push(ListItem::new(""));
                }
                items.push(ListItem::new(self.day_label(slot.date)));
            }
            let (marker, style) = match (self.state(slot), timing(&self.cfg, &slot.meal, slot.date, now)) {
                (Loaded::Loading, _) => (" …", Style::new().dim()),
                (Loaded::Empty, _) => (" –", Style::new().dim()),
                (_, Timing::Over) => ("  ", Style::new().dim()),
                (_, Timing::Now { .. }) => (" ●", Style::new().fg(Color::Green)),
                (_, Timing::Upcoming { .. }) => ("  ", Style::new()),
            };
            if i == self.sel {
                sel_row = items.len();
            }
            items.push(ListItem::new(Line::from(vec![
                Span::styled(format!(" {:<10}{:>7}", slot.meal.label, short_time(slot.meal.start)), style),
                Span::styled(marker, style.remove_modifier(Modifier::DIM)),
            ])));
        }
        let highlight = if focused {
            Style::new().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::new().bg(Color::DarkGray)
        };
        let list = List::new(items)
            .block(
                Block::new()
                    .borders(Borders::RIGHT)
                    .border_style(if focused { Style::new().fg(ACCENT) } else { Style::new().dim() }),
            )
            .highlight_style(highlight);
        let mut state = ListState::default().with_selected(Some(sel_row));
        f.render_stateful_widget(list, area, &mut state);
    }

    fn draw_menu(&mut self, f: &mut Frame, area: Rect, now: DateTime<Tz>) {
        let area = area.inner(ratatui::layout::Margin { horizontal: 2, vertical: 0 });
        let Some(slot) = self.slots.get(self.sel) else {
            f.render_widget(Paragraph::new("No meals scheduled in the lookahead window (see config)."), area);
            return;
        };
        let [head, body] = Layout::vertical([Constraint::Length(4), Constraint::Fill(1)]).areas(area);

        // ---- header: meal, date/time, status (+ progress bar while serving)
        let focused = self.focus == Focus::Menu;
        let title_style = if focused { Style::new().fg(Color::Black).bg(ACCENT) } else { Style::new().fg(ACCENT) };
        let mut head_lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled(format!(" {} ", slot.meal.label), title_style.add_modifier(Modifier::BOLD)),
                Span::raw(format!("  {}", slot.date.format("%A, %B %-d"))),
                Span::from(format!("  ·  {} – {}", fmt_time(slot.meal.start), fmt_time(slot.meal.end))).dim(),
            ]),
        ];
        head_lines.push(match timing(&self.cfg, &slot.meal, slot.date, now) {
            Timing::Now { left_secs } => {
                let total = (self.cfg.at(slot.date, slot.meal.end) - self.cfg.at(slot.date, slot.meal.start))
                    .num_seconds()
                    .max(1);
                let w = 24usize;
                let done = (((total - left_secs) as f64 / total as f64) * w as f64).round() as usize;
                Line::from(vec![
                    " ● serving now  ".fg(Color::Green).bold(),
                    "━".repeat(done.min(w)).fg(Color::Green),
                    "─".repeat(w - done.min(w)).dim(),
                    format!("  {} left", fmt_duration(left_secs)).fg(Color::Green),
                ])
            }
            Timing::Upcoming { in_secs } => Line::from(format!(" starts in {}", fmt_duration(in_secs))).fg(Color::Yellow),
            Timing::Over => Line::from(" ended").dim(),
        });
        f.render_widget(Paragraph::new(head_lines), head);

        // ---- body: station cards, laid out in columns, scrolled as one canvas
        let mut note: Option<Line> = None;
        let cards: Vec<Station> = match self.menus.meal(&slot.meal, slot.date) {
            None => {
                f.render_widget(Paragraph::new(" loading…").dim(), body);
                return;
            }
            Some(st) if st.is_empty() => {
                f.render_widget(Paragraph::new(" No menu posted.").dim(), body);
                return;
            }
            Some(stations) => {
                let main = main_only(&self.cfg, &stations);
                if self.show_all {
                    main_first(&self.cfg, &stations)
                } else if main.is_empty() {
                    note = Some(Line::from(format!(
                        " No {} this meal, so showing every station.",
                        self.cfg.main_stations.join(" / ")
                    )).dim());
                    stations
                } else {
                    let hidden = stations.len() - main.len();
                    if hidden > 0 {
                        note = Some(Line::from(vec![
                            format!(" + {hidden} more station{}  ", if hidden == 1 { "" } else { "s" }).dim(),
                            "a".fg(ACCENT),
                            " to show all".dim(),
                        ]));
                    }
                    main
                }
            }
        };

        let canvas = self.layout_cards(&cards, body.width.saturating_sub(1), note);
        let content_h = canvas.area.height;
        self.view_h = body.height;
        let max_scroll = content_h.saturating_sub(body.height);
        self.scroll = self.scroll.min(max_scroll);

        let buf = f.buffer_mut();
        for y in 0..body.height.min(content_h - self.scroll.min(content_h)) {
            for x in 0..canvas.area.width.min(body.width) {
                if let (Some(src), Some(dst)) = (canvas.cell((x, y + self.scroll)), buf.cell_mut((body.x + x, body.y + y))) {
                    *dst = src.clone();
                }
            }
        }
        if max_scroll > 0 {
            let mut sb = ScrollbarState::new(max_scroll as usize).position(self.scroll as usize);
            f.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None)
                    .thumb_style(if focused { Style::new().fg(ACCENT) } else { Style::new().dim() })
                    .track_style(Style::new().dim()),
                body,
                &mut sb,
            );
        }
    }

    /// Render station cards into an off-screen buffer `width` wide, masonry
    /// style (each card goes in the currently shortest column).
    fn layout_cards(&self, cards: &[Station], width: u16, note: Option<Line>) -> Buffer {
        let cols = (width / CARD_MIN_W).clamp(1, 3).min(cards.len().max(1) as u16);
        let card_w = ((width - (cols - 1)) / cols).min(CARD_MAX_W).max(10);
        let inner_w = (card_w - 4) as usize; // borders + 1 padding each side

        let rendered: Vec<(Vec<Line>, bool)> =
            cards.iter().map(|s| (card_lines(s, inner_w, self.details), is_main(&self.cfg, s))).collect();
        let mut heights = vec![0u16; cols as usize];
        let mut placed = Vec::new();
        for (i, (lines, _)) in rendered.iter().enumerate() {
            let col = (0..cols as usize).min_by_key(|&c| heights[c]).unwrap();
            let h = lines.len() as u16 + 2;
            placed.push((i, Rect::new(col as u16 * (card_w + 1), heights[col], card_w, h)));
            heights[col] += h;
        }
        let note_y = heights.iter().copied().max().unwrap_or(0);
        let total_h = note_y + if note.is_some() { 2 } else { 0 };

        let mut buf = Buffer::empty(Rect::new(0, 0, width.max(1), total_h.max(1)));
        for (i, rect) in placed {
            let (lines, main) = &rendered[i];
            let title_style = if *main { Style::new().fg(ACCENT).bold() } else { Style::new().bold() };
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(if *main { Style::new().fg(ACCENT) } else { Style::new().dim() })
                .title(Span::styled(format!(" {} ", cards[i].name), title_style))
                .padding(ratatui::widgets::Padding::horizontal(1));
            Paragraph::new(lines.clone()).block(block).render(rect, &mut buf);
        }
        if let Some(note) = note {
            note.render(Rect::new(0, note_y + 1, width, 1), &mut buf);
        }
        buf
    }

    fn draw_config(&self, f: &mut Frame) {
        let text = self.cfg.describe();
        let area = f.area();
        let h = (text.lines().count() as u16 + 2).min(area.height);
        let w = area.width.saturating_sub(4).min(text.lines().map(|l| l.len() as u16 + 4).max().unwrap_or(20));
        let rect = Rect::new((area.width - w) / 2, (area.height - h) / 2, w, h);
        f.render_widget(Clear, rect);
        f.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: false }).block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::new().fg(ACCENT))
                    .title(" config (c to close) "),
            ),
            rect,
        );
    }
}

/// Lines for one station card, `w` columns wide.
fn card_lines(st: &Station, w: usize, details: bool) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let has_extra = |i: &Item| details && (i.desc.is_some() || !i.allergens.is_empty());
    for (n, item) in st.items.iter().enumerate() {
        // Space out items that carry detail lines; keep plain ones compact.
        if n > 0 && (has_extra(item) || has_extra(&st.items[n - 1])) {
            out.push(Line::raw(""));
        }
        let badge = match item.diet {
            Some(Diet::Vegan) => Some(Span::from("VG").green().bold()),
            Some(Diet::Vegetarian) => Some(Span::from("V").green()),
            None => None,
        };
        let cal = item.calories.map(|c| format!("{c} cal")).unwrap_or_default();
        let badge_w = badge.as_ref().map_or(0, |b| b.width() + 1);
        let name_w = w.saturating_sub(badge_w + cal.len() + 2).max(8);
        let name_lines = wrap(&item.name, name_w);
        for (j, text) in name_lines.iter().enumerate() {
            let mut spans = vec![Span::raw(text.clone())];
            let mut used = text.chars().count();
            if j == 0 {
                if let Some(b) = &badge {
                    spans.push(Span::raw(" "));
                    spans.push(b.clone());
                    used += badge_w;
                }
                if !cal.is_empty() {
                    spans.push(Span::raw(" ".repeat(w.saturating_sub(used + cal.len()).max(1))));
                    spans.push(Span::from(cal.clone()).dim());
                }
            }
            out.push(Line::from(spans));
        }
        if details {
            if let Some(desc) = &item.desc {
                out.extend(wrap(desc, w.saturating_sub(2)).into_iter().map(|l| Line::from(format!("  {l}")).dim().italic()));
            }
            if !item.allergens.is_empty() {
                let a = format!("contains {}", item.allergens.join(", "));
                out.extend(wrap(&a, w.saturating_sub(2)).into_iter().map(|l| Line::from(format!("  {l}")).fg(Color::Yellow).dim()));
            }
        }
    }
    out
}

/// Greedy word wrap; words longer than `w` are hard-split.
fn wrap(s: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let mut word: Vec<char> = word.chars().collect();
        while word.len() > w {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            lines.push(word.drain(..w).collect());
        }
        let word: String = word.into_iter().collect();
        if cur.is_empty() {
            cur = word;
        } else if cur.chars().count() + 1 + word.chars().count() <= w {
            cur.push(' ');
            cur.push_str(&word);
        } else {
            lines.push(std::mem::replace(&mut cur, word));
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

fn short_time(t: NaiveTime) -> String {
    let s = t.format("%-I:%M%P").to_string().replace(":00", "");
    s.trim_end_matches('m').to_string()
}
