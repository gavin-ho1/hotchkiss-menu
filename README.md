# hotchkiss-menu

The Hotchkiss dining hall menu in your terminal, pulled from
[Nutrislice](https://hotchkiss.nutrislice.com/menu/dining-hall).

- **`hotchkissdininghall`** (also `dh`, `dininghall`): interactive TUI
- **`hotchkissmenu`**: plain CLI, good for a quick check or scripts

Both show the **next meal** by default (or the one being served now), and only
the main stations: Chef's Table, or Wakin' Up for breakfast and brunch.

## Install

Needs Rust (`brew install rust`).

```sh
cargo install --path .
mkdir -p ~/.config/hotchkiss-menu
cp config.example.toml ~/.config/hotchkiss-menu/config.toml

# optional short names for the TUI
ln -s ~/.cargo/bin/hotchkissdininghall ~/.local/bin/dh
ln -s ~/.cargo/bin/hotchkissdininghall ~/.local/bin/dininghall
```

## TUI

```
dh
```

Meals are listed on the left, and the selected meal's menu is on the right.

| Key | Meal list | Menu |
|---|---|---|
| `j` / `k` | change meal | scroll |
| `l` / `h` | open menu | back to list |
| `^d` / `^u`, `g` / `G` | | half page, top / bottom |
| `[` / `]` | previous / next day | previous / next day |
| `n` | jump to next meal | jump to next meal |
| `a` | toggle all stations | toggle all stations |
| `i` | toggle descriptions + allergens | toggle descriptions + allergens |
| `r` / `c` / `q` | refresh / config / quit | refresh / config / quit |

The mouse works too: click a meal or day to open it, click the menu to focus
it, scroll with the wheel, and click any key in the bottom bar to run it.

**VG** = vegan, **V** = vegetarian.

## CLI

```sh
hotchkissmenu                      # next meal
hotchkissmenu -n 3                 # next three meals
hotchkissmenu -d tomorrow          # every meal tomorrow
hotchkissmenu -d sunday -m brunch  # one meal on a day
hotchkissmenu -t 6pm               # the meal at/after 6pm today
hotchkissmenu -a -i                # all stations, with descriptions + allergens
```

Dates can be `today`, `tomorrow`, `sunday`, `+2`, `10/30`, or `2026-10-30`.
See `hotchkissmenu --help` for everything.

## Config

`~/.config/hotchkiss-menu/config.toml` sets meal times, which stations count as
"main", and caching. It's strict: missing or unknown keys are errors, and the
resolved config is printed on every run.

> **Meal times are not published by Nutrislice.** The ones in
> `config.example.toml` are estimates; edit them to match the real schedule.

Sunday brunch is spread across three Nutrislice menus (breakfast, brunch and
lunch), so the config merges them into one meal.

## How it works

Nutrislice serves a week of menus per meal type as JSON
(`/menu/api/weeks/school/dining-hall/menu-type/<type>/<y>/<m>/<d>/`). Weeks are
cached in `~/.cache/hotchkiss-menu` (30 min by default), so the TUI opens
instantly and refreshes in the background. Duplicate numbered stations
("Chef's Table 2") are folded into the first one.
