//! Shared core for the Hotchkiss dining hall CLI (`hotchkissmenu`) and TUI
//! (`hotchkissdininghall`): config, fetching + disk cache, and turning
//! Nutrislice menus into meals and stations.

use chrono::{DateTime, Datelike, Days, NaiveDate, NaiveTime, TimeZone};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const WEEKDAYS: [&str; 7] =
    ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];

// ------------------------------------------------------------------ config

// Raw TOML shape. No serde defaults anywhere: a missing key is an error, and
// deny_unknown_fields rejects typos/leftovers.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    source: RawSource,
    behavior: RawBehavior,
    diet: RawDiet,
    schedule: RawSchedule,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDiet {
    avoid: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSource {
    base_url: String,
    school: String,
    timezone: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBehavior {
    lookahead_days: u32,
    main_stations: Vec<String>,
    cache_ttl_minutes: u64,
    cache_dir: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSchedule {
    monday: Vec<RawMeal>,
    tuesday: Vec<RawMeal>,
    wednesday: Vec<RawMeal>,
    thursday: Vec<RawMeal>,
    friday: Vec<RawMeal>,
    saturday: Vec<RawMeal>,
    sunday: Vec<RawMeal>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMeal {
    label: String,
    menus: Vec<String>,
    start: String,
    end: String,
}

#[derive(Clone, Debug)]
pub struct Meal {
    pub label: String,
    pub menus: Vec<String>,
    pub start: NaiveTime,
    pub end: NaiveTime,
}

pub struct Config {
    pub path: PathBuf,
    pub base_url: String,
    pub school: String,
    pub tz: Tz,
    pub lookahead_days: u32,
    pub main_stations: Vec<String>,
    pub cache_ttl: Duration,
    pub cache_dir: PathBuf,
    /// Allergens to flag, lowercase ("sesame", "shellfish", ...).
    pub avoid: Vec<String>,
    /// Indexed Monday = 0.
    pub schedule: [Vec<Meal>; 7],
}

fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| "HOME is not set".to_string())
}

pub fn default_config_path() -> Result<PathBuf, String> {
    Ok(home()?.join(".config/hotchkiss-menu/config.toml"))
}

fn hhmm(s: &str, at: &str) -> Result<NaiveTime, String> {
    let ok = s.len() == 5 && s.as_bytes()[2] == b':';
    ok.then(|| NaiveTime::parse_from_str(s, "%H:%M").ok())
        .flatten()
        .ok_or_else(|| format!("{at}: {s:?} is not a 24h \"HH:MM\" time"))
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        let text = fs::read_to_string(path)
            .map_err(|e| format!("can't read config {}: {e}", path.display()))?;
        let raw: RawConfig =
            toml::from_str(&text).map_err(|e| format!("{}:\n{e}", path.display()))?;

        let tz: Tz = raw.source.timezone.parse().map_err(|_| {
            format!("source.timezone: unknown timezone {:?}", raw.source.timezone)
        })?;
        let b = raw.behavior;
        if b.lookahead_days < 1 {
            return Err("behavior.lookahead_days: must be >= 1".into());
        }
        if b.main_stations.is_empty() || b.main_stations.iter().any(|s| s.trim().is_empty()) {
            return Err("behavior.main_stations: must be a non-empty list of station names".into());
        }
        let cache_dir = match b.cache_dir.strip_prefix("~/") {
            Some(rest) => home()?.join(rest),
            None => PathBuf::from(&b.cache_dir),
        };

        if raw.diet.avoid.iter().any(|a| a.trim().is_empty()) {
            return Err("diet.avoid: entries must be non-empty allergen names (use [] for none)".into());
        }
        let avoid = raw.diet.avoid.iter().map(|a| a.trim().to_lowercase()).collect();

        let s = raw.schedule;
        let days = [s.monday, s.tuesday, s.wednesday, s.thursday, s.friday, s.saturday, s.sunday];
        let mut schedule: [Vec<Meal>; 7] = Default::default();
        for (i, entries) in days.into_iter().enumerate() {
            for (j, e) in entries.into_iter().enumerate() {
                let at = format!("schedule.{}[{j}]", WEEKDAYS[i]);
                if e.menus.is_empty() || e.menus.iter().any(|m| m.trim().is_empty()) {
                    return Err(format!("{at}.menus: must be a non-empty list of menu types"));
                }
                let start = hhmm(&e.start, &format!("{at}.start"))?;
                let end = hhmm(&e.end, &format!("{at}.end"))?;
                if end <= start {
                    return Err(format!("{at}: end {} is not after start {}", e.end, e.start));
                }
                if schedule[i].last().is_some_and(|prev: &Meal| start < prev.end) {
                    return Err(format!(
                        "{at}: starts before the previous meal ends (keep meals in order, non-overlapping)"
                    ));
                }
                schedule[i].push(Meal { label: e.label, menus: e.menus, start, end });
            }
        }

        Ok(Config {
            path: path.to_path_buf(),
            base_url: raw.source.base_url.trim_end_matches('/').to_string(),
            school: raw.source.school,
            tz,
            lookahead_days: b.lookahead_days,
            main_stations: b.main_stations,
            cache_ttl: Duration::from_secs(b.cache_ttl_minutes * 60),
            cache_dir,
            avoid,
            schedule,
        })
    }

    /// The fully-resolved config, for printing at startup.
    pub fn describe(&self) -> String {
        let mut out = vec![
            format!("config: {}", self.path.display()),
            format!("  source: {}  school={}  tz={}", self.base_url, self.school, self.tz),
            format!(
                "  lookahead_days={}  main_stations={:?}  cache_ttl={}m  cache_dir={}",
                self.lookahead_days,
                self.main_stations,
                self.cache_ttl.as_secs() / 60,
                self.cache_dir.display()
            ),
            format!("  avoid={:?}", self.avoid),
        ];
        for (i, meals) in self.schedule.iter().enumerate() {
            let line = meals
                .iter()
                .map(|m| {
                    format!("{} {}-{} [{}]", m.label, m.start.format("%H:%M"),
                            m.end.format("%H:%M"), m.menus.join("+"))
                })
                .collect::<Vec<_>>()
                .join("  ");
            out.push(format!("  {}: {}", &WEEKDAYS[i][..3],
                             if line.is_empty() { "(closed)".into() } else { line }));
        }
        out.join("\n")
    }

    pub fn meals_on(&self, d: NaiveDate) -> &[Meal] {
        &self.schedule[d.weekday().num_days_from_monday() as usize]
    }

    pub fn all_menu_types(&self) -> Vec<String> {
        let mut v: Vec<String> =
            self.schedule.iter().flatten().flat_map(|m| m.menus.iter().cloned()).collect();
        v.sort();
        v.dedup();
        v
    }

    pub fn now(&self) -> DateTime<Tz> {
        chrono::Utc::now().with_timezone(&self.tz)
    }

    /// Local wall-clock time on `d` (nudged forward past a DST gap).
    pub fn at(&self, d: NaiveDate, t: NaiveTime) -> DateTime<Tz> {
        let naive = d.and_time(t);
        self.tz
            .from_local_datetime(&naive)
            .earliest()
            .or_else(|| self.tz.from_local_datetime(&(naive + chrono::Duration::hours(1))).earliest())
            .expect("valid local time")
    }

    fn is_main(&self, station: &str) -> bool {
        let f = family(station);
        self.main_stations.iter().any(|m| family(m) == f)
    }
}

// ------------------------------------------------------------------ stations

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Station {
    pub name: String,
    pub items: Vec<Item>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum Diet {
    Vegan,
    Vegetarian,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Item {
    pub name: String,
    pub desc: Option<String>,
    pub diet: Option<Diet>,
    /// Lowercase allergen names: "milk", "wheat", "gluten", ...
    pub allergens: Vec<String>,
    pub calories: Option<u32>,
}

/// "honey roasted carrots" -> "Honey Roasted Carrots". Only touches names with
/// no capitals at all, so the kitchen's own capitalization is kept.
fn tidy_name(s: &str) -> String {
    let s = s.trim();
    if s.chars().any(char::is_uppercase) {
        return s.to_string();
    }
    const SMALL: [&str; 9] = ["a", "an", "and", "in", "of", "on", "or", "the", "with"];
    s.split(' ')
        .enumerate()
        .map(|(i, w)| if i > 0 && SMALL.contains(&w) { w.to_string() } else { capitalize(w) })
        .collect::<Vec<_>>()
        .join(" ")
}

fn capitalize(w: &str) -> String {
    let mut c = w.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

fn normalize(name: &str) -> String {
    name.trim().replace(['\u{2019}', '\u{2018}'], "'").to_lowercase()
}

fn strip_number(name: &str) -> &str {
    match name.trim().rsplit_once(' ') {
        Some((base, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => base.trim_end(),
        _ => name.trim(),
    }
}

/// "Chef's Table 2" and "Chef’s Table" are the same family: "chef's table".
pub fn family(name: &str) -> String {
    normalize(strip_number(name))
}

/// (menu type, Sunday that starts the week). Nutrislice serves Sun..Sat weeks.
pub type WeekKey = (String, NaiveDate);
/// "YYYY-MM-DD" -> stations in menu order.
pub type Week = HashMap<String, Vec<Station>>;

pub fn week_start(d: NaiveDate) -> NaiveDate {
    d - Days::new(d.weekday().num_days_from_sunday() as u64)
}

pub fn keys_for<'a>(dates: impl IntoIterator<Item = NaiveDate>, menus: impl IntoIterator<Item = &'a String> + Clone) -> Vec<WeekKey> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for d in dates {
        for m in menus.clone() {
            let k = (m.clone(), week_start(d));
            if seen.insert(k.clone()) {
                out.push(k);
            }
        }
    }
    out
}

#[derive(Default)]
pub struct Menus {
    weeks: HashMap<WeekKey, Week>,
}

impl Menus {
    pub fn insert(&mut self, key: WeekKey, week: Week) {
        self.weeks.insert(key, week);
    }

    pub fn has(&self, key: &WeekKey) -> bool {
        self.weeks.contains_key(key)
    }

    fn day(&self, menu: &str, d: NaiveDate) -> Option<&[Station]> {
        let week = self.weeks.get(&(menu.to_string(), week_start(d)))?;
        Some(week.get(&d.to_string()).map(Vec::as_slice).unwrap_or(&[]))
    }

    pub fn has_food(&self, menu: &str, d: NaiveDate) -> bool {
        self.day(menu, d).is_some_and(|s| s.iter().any(|st| !st.items.is_empty()))
    }

    /// All stations for a meal: its menu types merged, duplicate items removed,
    /// and numbered copies collapsed ("Chef's Table" + "Chef's Table 2" -> the
    /// first one seen, shown as "Chef's Table"). `None` if data isn't loaded yet.
    pub fn meal(&self, meal: &Meal, d: NaiveDate) -> Option<Vec<Station>> {
        let mut out: Vec<Station> = Vec::new();
        let mut seen_items: Vec<HashSet<String>> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new(); // family -> out idx
        let mut family_size: HashMap<String, HashSet<String>> = HashMap::new();
        let mut taken: HashMap<String, String> = HashMap::new(); // family -> exact name kept

        for menu in &meal.menus {
            for st in self.day(menu, d)? {
                let fam = family(&st.name);
                family_size.entry(fam.clone()).or_default().insert(normalize(&st.name));
                // Keep only the first-seen member of a family; later numbered
                // copies are dropped. Same exact name from another menu merges.
                let kept = taken.entry(fam.clone()).or_insert_with(|| normalize(&st.name));
                if *kept != normalize(&st.name) {
                    continue;
                }
                let i = *index.entry(fam).or_insert_with(|| {
                    out.push(Station { name: st.name.trim().to_string(), items: Vec::new() });
                    seen_items.push(HashSet::new());
                    out.len() - 1
                });
                for item in &st.items {
                    if seen_items[i].insert(item.name.to_lowercase()) {
                        out[i].items.push(item.clone());
                    }
                }
            }
        }
        for st in &mut out {
            if family_size[&family(&st.name)].len() > 1 {
                st.name = strip_number(&st.name).to_string();
            }
        }
        out.retain(|s| !s.items.is_empty());
        Some(out)
    }
}

/// The item's allergens that are on the config's avoid list. Matches whole
/// words, ignoring plurals: "nut" flags "tree nuts", "peanut" flags "peanuts".
pub fn avoided<'a>(cfg: &Config, item: &'a Item) -> Vec<&'a str> {
    fn words(s: &str) -> Vec<String> {
        s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(|w| w.trim_end_matches('s').to_string()).collect()
    }
    item.allergens
        .iter()
        .filter(|a| {
            let a = words(a);
            cfg.avoid.iter().any(|v| {
                let v = words(v);
                !v.is_empty() && a.windows(v.len()).any(|w| w == v.as_slice())
            })
        })
        .map(String::as_str)
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DietFilter {
    All,
    Vegetarian,
    Vegan,
}

impl DietFilter {
    pub fn allows(self, item: &Item) -> bool {
        match self {
            DietFilter::All => true,
            DietFilter::Vegetarian => item.diet.is_some(),
            DietFilter::Vegan => item.diet == Some(Diet::Vegan),
        }
    }
}

/// Stations with only the items the filter allows; stations left empty are dropped.
pub fn filter_diet(stations: Vec<Station>, f: DietFilter) -> Vec<Station> {
    stations
        .into_iter()
        .map(|mut s| {
            s.items.retain(|i| f.allows(i));
            s
        })
        .filter(|s| !s.items.is_empty())
        .collect()
}

pub fn is_main(cfg: &Config, station: &Station) -> bool {
    cfg.is_main(&station.name)
}

/// Every station, main stations first, otherwise in menu order.
pub fn main_first(cfg: &Config, stations: &[Station]) -> Vec<Station> {
    let (mut main, rest): (Vec<Station>, Vec<Station>) = stations.iter().cloned().partition(|s| cfg.is_main(&s.name));
    main.extend(rest);
    main
}

/// Just the configured main stations ("the main food"), in menu order.
pub fn main_only(cfg: &Config, stations: &[Station]) -> Vec<Station> {
    stations.iter().filter(|s| cfg.is_main(&s.name)).cloned().collect()
}

// ------------------------------------------------------------------ fetch + cache

#[derive(Deserialize)]
struct ApiWeek {
    days: Vec<ApiDay>,
}

#[derive(Deserialize)]
struct ApiDay {
    date: String,
    menu_items: Vec<ApiItem>,
}

#[derive(Deserialize)]
struct ApiItem {
    #[serde(default)]
    is_station_header: bool,
    #[serde(default)]
    is_section_title: bool,
    text: Option<String>,
    food: Option<ApiFood>,
}

#[derive(Deserialize)]
struct ApiFood {
    name: Option<String>,
    description: Option<String>,
    icons: Option<ApiIcons>,
    rounded_nutrition_info: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct ApiIcons {
    #[serde(default)]
    food_icons: Vec<ApiIcon>,
}

#[derive(Deserialize)]
struct ApiIcon {
    slug: String,
    name: String,
    #[serde(default)]
    is_filter: bool,
}

fn to_item(f: ApiFood) -> Option<Item> {
    let name = tidy_name(f.name.as_deref()?);
    if name.is_empty() {
        return None;
    }
    let icons = f.icons.map(|i| i.food_icons).unwrap_or_default();
    let has = |slug: &str| icons.iter().any(|i| i.slug == slug);
    let diet = if has("vegan") {
        Some(Diet::Vegan)
    } else if has("vegetarian") {
        Some(Diet::Vegetarian)
    } else {
        None
    };
    // Nutrislice marks allergens as the "filter" icons.
    let allergens = icons
        .iter()
        .filter(|i| i.is_filter)
        .map(|i| i.name.trim().trim_start_matches("Contains ").to_lowercase())
        .collect();
    let desc = f
        .description
        .map(|d| capitalize(d.trim()))
        .filter(|d| !d.is_empty() && !d.eq_ignore_ascii_case(&name));
    let calories = f
        .rounded_nutrition_info
        .as_ref()
        .and_then(|n| n.get("calories"))
        .and_then(|c| c.as_f64())
        .map(|c| c.round() as u32);
    Some(Item { name, desc, diet, allergens, calories })
}

fn parse_week(body: &str) -> Result<Week, String> {
    let api: ApiWeek = serde_json::from_str(body).map_err(|e| format!("unexpected API response: {e}"))?;
    let mut week = Week::new();
    for day in api.days {
        let mut stations: Vec<Station> = Vec::new();
        for it in day.menu_items {
            let text = it.text.as_deref().unwrap_or("").trim();
            if it.is_station_header || it.is_section_title {
                stations.push(Station {
                    name: if text.is_empty() { "Other".into() } else { text.into() },
                    items: Vec::new(),
                });
                continue;
            }
            let Some(item) = it.food.and_then(to_item) else { continue };
            if stations.is_empty() {
                stations.push(Station { name: "Other".into(), items: Vec::new() });
            }
            let st = stations.last_mut().unwrap();
            if !st.items.iter().any(|i| i.name.eq_ignore_ascii_case(&item.name)) {
                st.items.push(item);
            }
        }
        week.insert(day.date, stations);
    }
    Ok(week)
}

fn cache_file(cfg: &Config, key: &WeekKey) -> PathBuf {
    cfg.cache_dir.join("v2").join(&cfg.school).join(&key.0).join(format!("{}.json", key.1))
}

/// Cached week and its age, whatever that age is.
pub fn read_cache(cfg: &Config, key: &WeekKey) -> Option<(Week, Duration)> {
    let path = cache_file(cfg, key);
    let age = fs::metadata(&path).ok()?.modified().ok()?.elapsed().unwrap_or(Duration::ZERO);
    let week = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
    Some((week, age))
}

pub fn fetch(cfg: &Config, key: &WeekKey) -> Result<Week, String> {
    let (menu, ws) = key;
    let url = format!(
        "{}/weeks/school/{}/menu-type/{}/{}/{:02}/{:02}/?format=json",
        cfg.base_url, cfg.school, menu, ws.year(), ws.month(), ws.day()
    );
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut resp = agent
        .get(&url)
        .header("User-Agent", "hotchkiss-menu/1.0")
        .call()
        .map_err(|e| format!("could not reach Nutrislice: {e}"))?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(format!("HTTP {status} for menu type {menu:?} (is it a valid slug?)\n  {url}"));
    }
    let body = resp
        .body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_string()
        .map_err(|e| format!("reading response for {menu:?}: {e}"))?;
    let week = parse_week(&body)?;

    // Best-effort atomic write; a failed cache write shouldn't block the menu.
    let path = cache_file(cfg, key);
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if fs::write(&tmp, serde_json::to_vec(&week).unwrap()).is_ok() {
        let _ = fs::rename(&tmp, &path);
    }
    Ok(week)
}

/// Fetch several weeks in parallel.
pub fn fetch_all(cfg: &Config, keys: &[WeekKey]) -> Vec<(WeekKey, Result<Week, String>)> {
    std::thread::scope(|s| {
        let handles: Vec<_> = keys.iter().map(|k| (k.clone(), s.spawn(move || fetch(cfg, k)))).collect();
        handles
            .into_iter()
            .map(|(k, h)| (k, h.join().unwrap_or_else(|_| Err("fetch thread panicked".into()))))
            .collect()
    })
}

/// Load `keys` into `menus`: a fresh cache entry is used as-is, otherwise the
/// week is fetched. If a fetch fails but a stale copy exists, the stale copy is
/// used and a warning returned. Errors if a week is unavailable entirely.
pub fn ensure(cfg: &Config, menus: &mut Menus, keys: &[WeekKey], refresh: bool) -> Result<Vec<String>, String> {
    let mut stale: HashMap<WeekKey, (Week, Duration)> = HashMap::new();
    let mut need = Vec::new();
    let missing: Vec<&WeekKey> = keys.iter().filter(|k| !menus.has(k)).collect();
    for k in missing {
        match read_cache(cfg, k) {
            Some((w, age)) if !refresh && age < cfg.cache_ttl => menus.insert(k.clone(), w),
            cached => {
                if let Some(c) = cached {
                    stale.insert(k.clone(), c);
                }
                need.push(k.clone());
            }
        }
    }
    let mut warnings = Vec::new();
    for (k, res) in fetch_all(cfg, &need) {
        match (res, stale.remove(&k)) {
            (Ok(w), _) => menus.insert(k, w),
            (Err(e), Some((w, age))) => {
                warnings.push(format!(
                    "warning: {e}\n  using cached {} menu for week of {} ({} old)",
                    k.0, k.1, fmt_duration(age.as_secs() as i64)
                ));
                menus.insert(k, w);
            }
            (Err(e), None) => return Err(e),
        }
    }
    Ok(warnings)
}

// ------------------------------------------------------------------ time helpers

pub fn fmt_time(t: NaiveTime) -> String {
    t.format("%-I:%M %p").to_string().replace(":00 ", " ")
}

pub fn fmt_duration(secs: i64) -> String {
    let mins = secs.max(0) / 60;
    let (d, h, m) = (mins / 1440, mins % 1440 / 60, mins % 60);
    let mut parts = Vec::new();
    if d > 0 { parts.push(format!("{d}d")); }
    if h > 0 { parts.push(format!("{h}h")); }
    if m > 0 && d == 0 { parts.push(format!("{m}m")); }
    if parts.is_empty() { "<1m".into() } else { parts.join(" ") }
}

pub enum Timing {
    Now { left_secs: i64 },
    Upcoming { in_secs: i64 },
    Over,
}

pub fn timing(cfg: &Config, meal: &Meal, d: NaiveDate, now: DateTime<Tz>) -> Timing {
    let (start, end) = (cfg.at(d, meal.start), cfg.at(d, meal.end));
    if now < start {
        Timing::Upcoming { in_secs: (start - now).num_seconds() }
    } else if now < end {
        Timing::Now { left_secs: (end - now).num_seconds() }
    } else {
        Timing::Over
    }
}

pub fn matches_meal(meal: &Meal, q: Option<&str>) -> bool {
    let Some(q) = q else { return true };
    let q = q.to_lowercase();
    meal.label.to_lowercase().starts_with(&q)
}

// ------------------------------------------------------------------ arg parsing

pub fn parse_date(s: &str, today: NaiveDate) -> Result<NaiveDate, String> {
    let s = s.trim().to_lowercase();
    let err = || format!("can't parse date {s:?} (try: today, tomorrow, sunday, +3, 10/30, 2026-10-30)");
    match s.as_str() {
        "today" | "now" => return Ok(today),
        "tomorrow" => return Ok(today + Days::new(1)),
        "yesterday" => return Ok(today - Days::new(1)),
        _ => {}
    }
    if let Some(n) = s.strip_suffix('d').unwrap_or(&s).strip_prefix(['+', '-']) {
        let n: u64 = n.parse().map_err(|_| err())?;
        return Ok(if s.starts_with('+') { today + Days::new(n) } else { today - Days::new(n) });
    }
    if s.len() >= 3 {
        if let Some(i) = WEEKDAYS.iter().position(|w| w.starts_with(&s)) {
            let ahead = (i as i64 - today.weekday().num_days_from_monday() as i64).rem_euclid(7);
            return Ok(today + Days::new(ahead as u64));
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(&s, "%Y-%m-%d") {
        return Ok(d);
    }
    let parts: Vec<&str> = s.split(['/', '-']).collect();
    let num = |p: &str| p.parse::<i32>().map_err(|_| err());
    match parts.as_slice() {
        [m, d] => NaiveDate::from_ymd_opt(today.year(), num(m)? as u32, num(d)? as u32),
        [m, d, y] => {
            let y = num(y)?;
            NaiveDate::from_ymd_opt(if y < 100 { y + 2000 } else { y }, num(m)? as u32, num(d)? as u32)
        }
        _ => None,
    }
    .ok_or_else(err)
}

pub fn parse_time(s: &str) -> Result<NaiveTime, String> {
    let s = s.trim().to_lowercase().replace(' ', "");
    let err = || format!("can't parse time {s:?} (try: 18:00, 6pm, 6:30pm, noon)");
    match s.as_str() {
        "noon" => return Ok(NaiveTime::from_hms_opt(12, 0, 0).unwrap()),
        "midnight" => return Ok(NaiveTime::MIN),
        _ => {}
    }
    let (body, ampm) = if let Some(b) = s.strip_suffix("am").or_else(|| s.strip_suffix('a')) {
        (b, Some(false))
    } else if let Some(b) = s.strip_suffix("pm").or_else(|| s.strip_suffix('p')) {
        (b, Some(true))
    } else {
        (s.as_str(), None)
    };
    let (h, m) = match body.split_once(':') {
        Some((h, m)) => (h, m),
        None if body.len() > 2 => body.split_at(body.len() - 2), // "630pm", "1830"
        None => (body, "0"),
    };
    let (mut h, m): (u32, u32) = (h.parse().map_err(|_| err())?, m.parse().map_err(|_| err())?);
    if let Some(pm) = ampm {
        if !(1..=12).contains(&h) {
            return Err(err());
        }
        h = h % 12 + if pm { 12 } else { 0 };
    }
    NaiveTime::from_hms_opt(h, m, 0).ok_or_else(err)
}
