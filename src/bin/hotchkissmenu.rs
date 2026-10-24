//! hotchkissmenu: print the Hotchkiss dining hall menu.

use chrono::{DateTime, Days, NaiveDate, NaiveTime};
use chrono_tz::Tz;
use clap::{Parser, ValueEnum};
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
  hotchkissmenu -m dinner            the next dinner
  hotchkissmenu --diet vegan         only vegan dishes
  hotchkissmenu --line               one line, for a status bar (tmux, SketchyBar)
  hotchkissmenu --json -n 3          machine-readable next three meals";

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
    /// Only show dishes for this diet
    #[arg(long, value_enum)]
    diet: Option<DietArg>,
    /// Print one line per meal (status bars): meal, time, a few dishes
    #[arg(long, conflicts_with = "json")]
    line: bool,
    /// Print the meals as JSON
    #[arg(long)]
    json: bool,
    /// Ignore the cache and re-download menus
    #[arg(short, long)]
    refresh: bool,
    /// Config file [default: ~/.config/hotchkiss-menu/config.toml]
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[derive(Clone, Copy, ValueEnum)]
enum DietArg {
    Vegetarian,
    Vegan,
}

impl Args {
    fn diet(&self) -> DietFilter {
        match self.diet {
            None => DietFilter::All,
            Some(DietArg::Vegetarian) => DietFilter::Vegetarian,
            Some(DietArg::Vegan) => DietFilter::Vegan,
        }
    }
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
    fn red(&self, s: &str) -> String { Self::sgr(self.out, "1;31", s) }
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
        if meals.is_empty() && !args.json {
            let what = meal_q.map_or("no meals".to_string(), |q| format!("no {q:?} meal"));
            println!("{what} scheduled on {} (per config)", d.format("%A %b %-d"));
        }
        let found: Vec<(&Meal, NaiveDate)> = meals.into_iter().map(|m| (m, d)).collect();
        output(&cfg, &menus, &found, now, &args, st);
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
    output(&cfg, &menus, &found, now, &args, st);
    Ok(())
}

fn output(cfg: &Config, menus: &Menus, found: &[(&Meal, NaiveDate)], now: DateTime<Tz>, args: &Args, st: &Style) {
    if args.json {
        let meals: Vec<_> = found.iter().map(|&(m, d)| to_json(cfg, menus, m, d, now, args)).collect();
        println!("{}", serde_json::to_string_pretty(&meals).unwrap());
    } else if args.line {
        for &(m, d) in found {
            println!("{}", one_line(cfg, menus, m, d, now, args));
        }
    } else {
        for &(m, d) in found {
            render(cfg, menus, m, d, now, args, st);
        }
    }
}

/// What a meal shows given -a and --diet: (shown, all after the diet filter,
/// whether there was no main station so every station is shown).
fn pick(cfg: &Config, menus: &Menus, meal: &Meal, d: NaiveDate, args: &Args) -> (Vec<Station>, Vec<Station>, bool) {
    let stations = filter_diet(menus.meal(meal, d).unwrap_or_default(), args.diet());
    if args.all {
        return (main_first(cfg, &stations), stations, false);
    }
    let main = main_only(cfg, &stations);
    if main.is_empty() { (stations.clone(), stations, true) } else { (main, stations, false) }
}

fn status_text(cfg: &Config, meal: &Meal, d: NaiveDate, now: DateTime<Tz>) -> (&'static str, Option<i64>) {
    match timing(cfg, meal, d, now) {
        Timing::Now { left_secs } => ("now", Some(left_secs)),
        Timing::Upcoming { in_secs } => ("upcoming", Some(in_secs)),
        Timing::Over => ("over", None),
    }
}

/// "Dinner 5:30 PM · 16m left · Baked Chicken; Honey Carrots +4" (";" since dish names can contain commas)
fn one_line(cfg: &Config, menus: &Menus, meal: &Meal, d: NaiveDate, now: DateTime<Tz>, args: &Args) -> String {
    const SHOW: usize = 2;
    let when = match status_text(cfg, meal, d, now) {
        ("now", Some(s)) => format!("{} left", fmt_duration(s)),
        ("upcoming", Some(s)) => format!("in {}", fmt_duration(s)),
        _ => "ended".into(),
    };
    let mut head = format!("{} {}", meal.label, fmt_time(meal.start));
    if d != now.date_naive() {
        head = format!("{} {head}", d.format("%a"));
    }
    let (shown, _, _) = pick(cfg, menus, meal, d, args);
    let items: Vec<&Item> = shown.iter().flat_map(|s| &s.items).collect();
    if items.is_empty() {
        return format!("{head} · {when} · no menu posted");
    }
    let names: Vec<String> = items
        .iter()
        .take(SHOW)
        .map(|i| if avoided(cfg, i).is_empty() { i.name.clone() } else { format!("{} ⚠", i.name) })
        .collect();
    let more = if items.len() > SHOW { format!(" +{}", items.len() - SHOW) } else { String::new() };
    format!("{head} · {when} · {}{more}", names.join("; "))
}

fn to_json(cfg: &Config, menus: &Menus, meal: &Meal, d: NaiveDate, now: DateTime<Tz>, args: &Args) -> serde_json::Value {
    let (status, secs) = status_text(cfg, meal, d, now);
    let (shown, all, _) = pick(cfg, menus, meal, d, args);
    let stations: Vec<_> = shown
        .iter()
        .map(|s| {
            let items: Vec<_> = s
                .items
                .iter()
                .map(|i| {
                    serde_json::json!({
                        "name": i.name,
                        "description": i.desc,
                        "diet": i.diet.map(|d| match d { Diet::Vegan => "vegan", Diet::Vegetarian => "vegetarian" }),
                        "calories": i.calories,
                        "allergens": i.allergens,
                        "avoid": avoided(cfg, i),
                    })
                })
                .collect();
            serde_json::json!({ "name": s.name, "main": is_main(cfg, s), "items": items })
        })
        .collect();
    serde_json::json!({
        "meal": meal.label,
        "date": d.to_string(),
        "start": meal.start.format("%H:%M").to_string(),
        "end": meal.end.format("%H:%M").to_string(),
        "status": status,
        "seconds": secs,
        "stations": stations,
        "hidden_stations": all.len() - shown.len(),
    })
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

    let (shown, stations, fell_back) = pick(cfg, menus, meal, d, args);
    if stations.is_empty() {
        let what = if menus.meal(meal, d).unwrap_or_default().is_empty() {
            "no menu posted"
        } else {
            "nothing matching --diet"
        };
        println!("  ({what})\n");
        return;
    }
    if fell_back {
        println!("  {}", st.dim(&format!("(no {} this meal; showing every station)", cfg.main_stations.join(" / "))));
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
            let bad = avoided(cfg, item);
            if bad.is_empty() {
                println!("    • {}{badge}{cal}", item.name);
            } else {
                println!("    • {}{badge}{cal}", st.red(&format!("{} ⚠", item.name)));
                if !args.info {
                    println!("      {}", st.red(&format!("contains {}", bad.join(", "))));
                }
            }
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
