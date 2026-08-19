//! What condition this device is in for syncing (Phase 6.7).
//!
//! Two things, and neither one can always be known:
//!
//! - **Whether the network is metered.** The operating system knows this for
//!   a cellular modem, and flags it on its own. For a phone's Wi-Fi hotspot
//!   it does **not**: Windows just sees another network there, and the only
//!   way it finds out is if someone marks it by hand once (Settings → Network
//!   → Wi-Fi → properties → "Metered connection"). It stays stuck to that
//!   network, so it's done once per network.
//! - **How much battery is left.** A desktop PC has none, and that's not an
//!   error: it's the answer. That's why everything is an `Option` — `None`
//!   means "unknown" or "not applicable", and it never translates into
//!   blocking the sync. Blocking on something that couldn't be measured
//!   would be the worst of both worlds.
//!
//! On Android none of this can be read from Rust (see `device_info.rs`: the
//! JNI context isn't initialized there). The screen reports it instead, since
//! it does have `navigator.getBattery()` and `navigator.connection`, via the
//! `report_conditions` command.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Conditions {
    /// `true` = the network is metered. `None` = couldn't find out.
    pub metered: Option<bool>,
    /// 0–100. `None` = no battery (a desktop PC) or unknown.
    pub battery_pct: Option<u8>,
    pub charging: Option<bool>,
}

impl Conditions {
    /// What this device can figure out on its own. On Android returns
    /// everything as `None`: the screen fills it in.
    pub fn read() -> Self {
        Conditions {
            metered: metered(),
            ..battery()
        }
    }
}

// ---------------------------------------------------------------------------
// Metered network
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
fn metered() -> Option<bool> {
    use windows::Networking::Connectivity::{NetworkCostType, NetworkInformation};

    // No internet profile means no network: that's not "not metered", it's unknown.
    let profile = NetworkInformation::GetInternetConnectionProfile().ok()?;
    let cost = profile.GetConnectionCost().ok()?;

    // Roaming and over the data limit are expensive even on a fixed plan.
    if cost.Roaming().unwrap_or(false) || cost.OverDataLimit().unwrap_or(false) {
        return Some(true);
    }
    match cost.NetworkCostType().ok()? {
        // `Fixed` is a capped plan; `Variable` is billed per megabyte.
        NetworkCostType::Fixed | NetworkCostType::Variable => Some(true),
        NetworkCostType::Unrestricted => Some(false),
        // `Unknown` is literally that.
        _ => None,
    }
}

#[cfg(not(target_os = "windows"))]
fn metered() -> Option<bool> {
    None
}

// ---------------------------------------------------------------------------
// Battery
// ---------------------------------------------------------------------------

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn battery() -> Conditions {
    let Ok(manager) = battery::Manager::new() else {
        return Conditions::default();
    };
    let Ok(mut batteries) = manager.batteries() else {
        return Conditions::default();
    };
    // No battery: a desktop PC. The answer is `None`, and that tells the
    // screen it doesn't need to offer the option.
    let Some(Ok(b)) = batteries.next() else {
        return Conditions::default();
    };
    let pct = (b.state_of_charge().value * 100.0).round().clamp(0.0, 100.0) as u8;
    Conditions {
        metered: None,
        battery_pct: Some(pct),
        charging: Some(b.state() == battery::State::Charging || b.state() == battery::State::Full),
    }
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn battery() -> Conditions {
    Conditions::default()
}

/// What this device knows right now, wherever it comes from.
///
/// On desktop it's measured on the spot (it's cheap and always up to date);
/// on Android it returns the last thing the screen reported. When the native
/// measurement doesn't know something, the reported value wins: preferring
/// our own `None` over real data from the other side would throw away the
/// only answer available.
pub fn current(state: &crate::AppState) -> Conditions {
    let reported = state.conditions.lock().map(|c| *c).unwrap_or_default();
    let native = Conditions::read();
    Conditions {
        metered: native.metered.or(reported.metered),
        battery_pct: native.battery_pct.or(reported.battery_pct),
        charging: native.charging.or(reported.charging),
    }
}

// ---------------------------------------------------------------------------
// The decision
// ---------------------------------------------------------------------------

/// This device's preferences. Local: they describe where it is and how much
/// battery it has, which is no other device's business.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    /// Sync with the server even if the network is metered.
    pub on_metered: bool,
    /// Below this percentage, don't sync automatically. `0` = no limit.
    pub min_battery: u8,
}

impl Default for Limits {
    fn default() -> Self {
        // Spending data without permission is one of the few things that
        // costs someone money, so the default is no.
        Limits { on_metered: false, min_battery: 20 }
    }
}

const SETTING_ON_METERED: &str = "sync_on_metered";
const SETTING_MIN_BATTERY: &str = "sync_min_battery";

impl Limits {
    pub fn load(conn: &rusqlite::Connection) -> Self {
        let d = Limits::default();
        Limits {
            on_metered: crate::db::get_setting(conn, SETTING_ON_METERED)
                .ok()
                .flatten()
                .map(|v| v == "1")
                .unwrap_or(d.on_metered),
            min_battery: crate::db::get_setting(conn, SETTING_MIN_BATTERY)
                .ok()
                .flatten()
                .and_then(|v| v.parse().ok())
                .map(|v: u8| v.min(100))
                .unwrap_or(d.min_battery),
        }
    }

    pub fn save(&self, conn: &rusqlite::Connection) -> anyhow::Result<()> {
        crate::db::set_setting(conn, SETTING_ON_METERED, if self.on_metered { "1" } else { "0" })?;
        crate::db::set_setting(conn, SETTING_MIN_BATTERY, &self.min_battery.to_string())?;
        Ok(())
    }
}

/// Why sync isn't running right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    Metered,
    Battery,
}

impl Hold {
    pub fn reason(&self) -> &'static str {
        match self {
            Hold::Metered => "the network is metered",
            Hold::Battery => "battery is low",
        }
    }
}

/// Whether **automatic** sync has to wait.
///
/// `remote` distinguishes the two limits, and the distinction matters:
/// moving a file to another device on the same network doesn't spend a
/// single byte of the data plan, even if the network is flagged as metered.
/// What costs money is going out to the internet, i.e. the server. Battery,
/// on the other hand, gets spent either way.
///
/// A manually requested sync never goes through here: you're the one asking
/// for it, looking at the screen, and it's the emergency exit for when the
/// operating system gets the network wrong.
pub fn hold(c: &Conditions, l: &Limits, remote: bool) -> Option<Hold> {
    if let (Some(pct), Some(false)) = (c.battery_pct, c.charging.or(Some(false))) {
        if l.min_battery > 0 && pct < l.min_battery {
            return Some(Hold::Battery);
        }
    }
    if remote && !l.on_metered && c.metered == Some(true) {
        return Some(Hold::Metered);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cond(metered: Option<bool>, pct: Option<u8>, charging: Option<bool>) -> Conditions {
        Conditions { metered, battery_pct: pct, charging }
    }

    #[test]
    fn knowing_nothing_does_not_hold() {
        // A desktop PC with an unidentified network. Blocking on something
        // that couldn't be measured would leave sync never running and never
        // saying why.
        assert_eq!(hold(&Conditions::default(), &Limits::default(), true), None);
    }

    #[test]
    fn metered_network_holds_the_server_but_not_the_local_network() {
        let c = cond(Some(true), None, None);
        let l = Limits::default();
        assert_eq!(hold(&c, &l, true), Some(Hold::Metered));
        assert_eq!(
            hold(&c, &l, false),
            None,
            "moving a file within the same network doesn't spend data"
        );
    }

    #[test]
    fn with_permission_set_metered_network_does_not_hold() {
        let c = cond(Some(true), None, None);
        let l = Limits { on_metered: true, ..Limits::default() };
        assert_eq!(hold(&c, &l, true), None);
    }

    #[test]
    fn low_battery_holds_everything_not_just_the_server() {
        let c = cond(Some(false), Some(9), Some(false));
        let l = Limits::default();
        assert_eq!(hold(&c, &l, true), Some(Hold::Battery));
        assert_eq!(hold(&c, &l, false), Some(Hold::Battery));
    }

    #[test]
    fn plugged_in_battery_left_does_not_matter() {
        let c = cond(Some(false), Some(3), Some(true));
        assert_eq!(hold(&c, &Limits::default(), true), None);
    }

    #[test]
    fn at_zero_the_battery_limit_is_off() {
        let c = cond(Some(false), Some(1), Some(false));
        let l = Limits { min_battery: 0, ..Limits::default() };
        assert_eq!(hold(&c, &l, true), None);
    }

    #[test]
    fn battery_wins_when_both_apply() {
        // The reason shown has to be the most urgent one: running out of
        // battery mid-transfer is worse than spending data.
        let c = cond(Some(true), Some(5), Some(false));
        assert_eq!(hold(&c, &Limits::default(), true), Some(Hold::Battery));
    }
}
