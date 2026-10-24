//! hotchkissmenu: print the Hotchkiss dining hall menu.

use chrono::{DateTime, Days, NaiveDate, NaiveTime};
use chrono_tz::Tz;
use clap::Parser;
use hotchkiss_menu::*;
use std::io::IsTerminal;
use std::path::PathBuf;

const EXAMPLES: &str = "\
Examples:
  hotchkissmenu                      main stations of the next (or current) meal
  hotchkissmenu -a                   ...with every station
  hotchkissmenu -i                   ...with descriptions and allergens
  hotchkissmenu -n 3                 next three meals
  hotchkissmenu -d tomorrow          every meal tomorrow
  hotchkissmenu -d sunday -m brunch  Sunday brunch
  hotchkissmenu -t 6pm               the meal at/after 6pm today
  hotchkissmenu -d 10/30 -t noon     the meal at/after noon on Oct 30
  hotchkissmenu -m dinner            the next dinner";

#[derive(Parser)]
#[command(
    name = "hotchkissmenu",
    about = "Hotchkiss dining hall menu. With no arguments, shows the main stations of the next meal.",
    after_help = EXAMPLES
)]
struct Args {
    /// Date: today, tomorrow, sunday, +2, 10/30, 2026-10-30. Alone, shows every meal that day
    #[arg(short, long)]
    date: Option<String>,
    /// Time: 18:00, 6pm, noon. Shows the meal at/after it
    #[arg(short, long)]
    time: Option<String>,
    /// Meal: breakfast, lunch, dinner, brunch (prefix ok)
    #[arg(short, long)]
    meal: Option<String>,
    /// Show the next N meals
    #[arg(short, long, value_name = "N")]
    next: Option<usize>,
    /// Show every station, not just the main ones
    #[arg(short, long)]
    all: bool,
    /// Show descriptions and allergens
    #[arg(short, long)]
    info: bool,
    /// Ignore the cache and re-download menus
    #[arg(short, long)]
    refresh: bool,
    /// Config file [default: ~/.config/hotchkiss-menu/config.toml]
    #[arg(short, long)]
    config: Option<PathBuf>,
}

struct Style {
    out: bool,
    err: bool,
}

impl Style {
    fn sgr(on: bool, code: &str, s: &str) -> String {
        if on { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }
    fn bold(&self, s: &str) -> String { Self::sgr(self.out, "1", s) }
    fn title(&self, s: &str) -> String { Self::sgr(self.out, "1;36", s) }
    fn yellow(&self, s: &str) -> String { Self::sgr(self.out, "33", s) }
    fn dim(&self, s: &str) -> String { Self::sgr(self.out, "2", s) }
    fn green(&self, s: &str) -> String { Self::sgr(self.out, "32", s) }
    fn italic_dim(&self, s: &str) -> String { Self::sgr(self.out, "2;3", s) }
    fn err_dim(&self, s: &str) -> String { Self::sgr(self.err, "2", s) }
    fn err_yellow(&self, s: &str) -> String { Self::sgr(self.err, "33", s) }
}

fn main() {
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let st = Style {
        out: std::io::stdout().is_terminal() && !no_color,
        err: std::io::stderr().is_terminal() && !no_color,
    };
    if let Err(e) = run(&st) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(st: &Style) -> Result<(), String> {
    let args = Args::parse();
    let path = match args.config.clone() {
        Some(p) => p,
        None => default_config_path()?,
    };
    let cfg = Config::load(&path).map_err(|e| format!("config: {e}"))?;
    eprintln!("{}\n", st.err_dim(&cfg.describe()));

    let now = cfg.now();
    let today = now.date_naive();
    let day = args.date.as_deref().map(|d| parse_date(d, today)).transpose()?;
    let time = args.time.as_deref().map(parse_time).transpose()?;
    if args.next == Some(0) {
        return Err("--next must be >= 1".into());
    }
    let meal_q = args.meal.as_deref();
    let mut menus = Menus::default();
    let warn = |w: Vec<String>| w.iter().for_each(|w| eprintln!("{}", st.err_yellow(w)));

    // Whole-day view: -d with no time and no -n.
    if let (Some(d), None, None) = (day, time, args.next) {
        let types = cfg.all_menu_types();
        warn(ensure(&cfg, &mut menus, &keys_for([d], &types), args.refresh)?);
        warn_unscheduled(&cfg, &menus, d, st);
        let meals: Vec<&Meal> = cfg.meals_on(d).iter().filter(|m| matches_meal(m, meal_q)).collect();
        if meals.is_empty() {
            let what = meal_q.map_or("no meals".to_string(), |q| format!("no {q:?} meal"));
            println!("{what} scheduled on {} (per config)", d.format("%A %b %-d"));
        }
        for m in meals {
            render(&cfg, &menus, m, d, now, &args, st);
        }
        return Ok(());
    }

    // "Next meal" view, from now or from the given date/time.
    let from = match (day, time) {
        (None, None) => now,
        _ => cfg.at(day.unwrap_or(today), time.unwrap_or(NaiveTime::MIN)),
    };
    let want = args.next.unwrap_or(1);
    let mut found: Vec<(&Meal, NaiveDate)> = Vec::new();
    'days: for offset in 0..=cfg.lookahead_days {
        let d = from.date_naive() + Days::new(offset as u64);
        let meals: Vec<&Meal> = cfg.meals_on(d).iter().filter(|m| matches_meal(m, meal_q)).collect();
        let types: Vec<String> = meals.iter().flat_map(|m| m.menus.iter().cloned()).collect();
        warn(ensure(&cfg, &mut menus, &keys_for([d], &types), args.refresh)?);
        for m in meals {
            if cfg.at(d, m.end) <= from || !m.menus.iter().any(|t| menus.has_food(t, d)) {
                continue;
            }
            found.push((m, d));
            if found.len() >= want {
                break 'days;
            }
        }
    }
    if found.is_empty() {
        let what = meal_q.map_or("meal".to_string(), |q| format!("{q:?} meal"));
        return Err(format!(
            "no {what} with a posted menu in the {} days after {}",
            cfg.lookahead_days,
            from.format("%a %b %-d %-I:%M %p")
        ));
    }
    for (m, d) in found {
        render(&cfg, &menus, m, d, now, &args, st);
    }
    Ok(())
}

fn render(cfg: &Config, menus: &Menus, meal: &Meal, d: NaiveDate, now: DateTime<Tz>, args: &Args, st: &Style) {
    let status = match timing(cfg, meal, d, now) {
        Timing::Now { left_secs } => st.yellow(&format!("  (serving now, {} left)", fmt_duration(left_secs))),
        Timing::Upcoming { in_secs } => st.yellow(&format!("  (in {})", fmt_duration(in_secs))),
        Timing::Over => String::new(),
    };
    println!(
        "{}  {} · {}–{}{status}",
        st.title(&meal.label),
        d.format("%a %b %-d"),
        fmt_time(meal.start),
        fmt_time(meal.end)
    );

    let stations = menus.meal(meal, d).unwrap_or_default();
    if stations.is_empty() {
        println!("  (no menu posted)\n");
        return;
    }
    let mut shown = if args.all { main_first(cfg, &stations) } else { main_only(cfg, &stations) };
    if shown.is_empty() {
        println!("  {}", st.dim(&format!("(no {} this meal; showing every station)", cfg.main_stations.join(" / "))));
        shown = stations.clone();
    }
    for s in &shown {
        println!("  {}", st.bold(&s.name));
        for item in &s.items {
            let badge = match item.diet {
                Some(Diet::Vegan) => format!(" {}", st.green("VG")),
                Some(Diet::Vegetarian) => format!(" {}", st.green("V")),
                None => String::new(),
            };
            let cal = item.calories.map_or(String::new(), |c| format!("  {}", st.dim(&format!("{c} cal"))));
            println!("    • {}{badge}{cal}", item.name);
            if args.info {
                if let Some(desc) = &item.desc {
                    println!("      {}", st.italic_dim(desc));
                }
                if !item.allergens.is_empty() {
                    println!("      {}", st.yellow(&format!("contains {}", item.allergens.join(", "))));
                }
            }
        }
    }
    let hidden = stations.len() - shown.len();
    if hidden > 0 {
        println!("  {}", st.dim(&format!("+ {hidden} more station(s): -a to show")));
    }
    println!();
}

/// Flag menu types that have food on `d` but aren't in that day's schedule.
fn warn_unscheduled(cfg: &Config, menus: &Menus, d: NaiveDate, st: &Style) {
    let used: Vec<&String> = cfg.meals_on(d).iter().flat_map(|m| &m.menus).collect();
    for t in cfg.all_menu_types() {
        if !used.contains(&&t) && menus.has_food(&t, d) {
            eprintln!(
                "{}",
                st.err_yellow(&format!(
                    "note: Nutrislice has a {t:?} menu on {d} that isn't in the {} schedule, so it isn't shown; check config",
                    d.format("%A").to_string().to_lowercase()
                ))
            );
        }
    }
}
