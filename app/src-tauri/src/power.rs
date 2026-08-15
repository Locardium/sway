//! En qué condiciones está este dispositivo para sincronizar (Fase 6.7).
//!
//! Dos cosas, y ninguna de las dos se puede saber siempre:
//!
//! - **Si la red se paga por dato.** El sistema operativo lo sabe para un
//!   módem celular, y lo marca solo. Para el hotspot de un teléfono por Wi-Fi
//!   **no**: ahí Windows ve una red más, y la única forma de que sepa es que
//!   alguien la marque a mano una vez (Configuración → Red → Wi-Fi →
//!   propiedades → "Conexión de uso medido"). Queda pegado a esa red, así que
//!   se hace una sola vez por red.
//! - **Cuánta batería queda.** Una PC de escritorio no tiene, y eso no es un
//!   error: es la respuesta. Por eso todo es `Option` — `None` significa "no
//!   se sabe" o "no aplica", y nunca se traduce en frenar el sync. Frenar por
//!   algo que no se pudo medir sería lo peor de los dos mundos.
//!
//! En Android nada de esto se puede leer desde Rust (ver `device_info.rs`: el
//! contexto de JNI no está inicializado). Lo reporta la pantalla, que sí tiene
//! `navigator.getBattery()` y `navigator.connection`, con el comando
//! `report_conditions`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Conditions {
    /// `true` = la red se paga por dato. `None` = no se pudo averiguar.
    pub metered: Option<bool>,
    /// 0–100. `None` = no hay batería (una PC de escritorio) o no se sabe.
    pub battery_pct: Option<u8>,
    pub charging: Option<bool>,
}

impl Conditions {
    /// Lo que este dispositivo puede averiguar por su cuenta. En Android
    /// devuelve todo en `None`: lo llena la pantalla.
    pub fn read() -> Self {
        Conditions {
            metered: metered(),
            ..battery()
        }
    }
}

// ---------------------------------------------------------------------------
// Red medida
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
fn metered() -> Option<bool> {
    use windows::Networking::Connectivity::{NetworkCostType, NetworkInformation};

    // Sin perfil de internet no hay red: no es "no medida", es que no se sabe.
    let profile = NetworkInformation::GetInternetConnectionProfile().ok()?;
    let cost = profile.GetConnectionCost().ok()?;

    // Roaming y pasado del límite son caros aunque el plan sea fijo.
    if cost.Roaming().unwrap_or(false) || cost.OverDataLimit().unwrap_or(false) {
        return Some(true);
    }
    match cost.NetworkCostType().ok()? {
        // `Fixed` es un plan con tope; `Variable` se paga por megabyte.
        NetworkCostType::Fixed | NetworkCostType::Variable => Some(true),
        NetworkCostType::Unrestricted => Some(false),
        // `Unknown` es literalmente eso.
        _ => None,
    }
}

#[cfg(not(target_os = "windows"))]
fn metered() -> Option<bool> {
    None
}

// ---------------------------------------------------------------------------
// Batería
// ---------------------------------------------------------------------------

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn battery() -> Conditions {
    let Ok(manager) = battery::Manager::new() else {
        return Conditions::default();
    };
    let Ok(mut batteries) = manager.batteries() else {
        return Conditions::default();
    };
    // Sin batería: una PC de escritorio. La respuesta es `None`, y con eso la
    // pantalla sabe que no tiene que ofrecer la opción.
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

/// Lo que sabe este dispositivo ahora mismo, venga de donde venga.
///
/// En desktop se mide en el momento (es barato y siempre está al día); en
/// Android se devuelve lo último que reportó la pantalla. Cuando la medición
/// nativa no sabe algo, gana lo reportado: preferir un `None` propio sobre un
/// dato real del otro lado sería tirar la única respuesta que hay.
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
// La decisión
// ---------------------------------------------------------------------------

/// Preferencias de este dispositivo. Locales: describen dónde está y con qué
/// batería, que no es asunto de ningún otro dispositivo.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    /// Sincronizar con el server aunque la red se pague por dato.
    pub on_metered: bool,
    /// Debajo de este porcentaje no se sincroniza solo. `0` = sin límite.
    pub min_battery: u8,
}

impl Default for Limits {
    fn default() -> Self {
        // Gastar datos sin permiso es de las pocas cosas que le cuestan plata a
        // alguien, así que el default es no.
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

/// Por qué no se sincroniza ahora.
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

/// Si el sync **automático** tiene que esperar.
///
/// `remote` distingue los dos límites, y la distinción importa: mover un
/// archivo a otro dispositivo de la misma red no gasta un solo byte del plan
/// de datos, aunque la red esté marcada como medida. Lo que se paga es salir a
/// internet, o sea el server. La batería, en cambio, se gasta igual.
///
/// Un sync pedido a mano nunca pasa por acá: lo estás pidiendo vos, mirando la
/// pantalla, y es la salida de emergencia cuando el sistema operativo se
/// equivoca sobre la red.
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
    fn sin_saber_nada_no_se_frena() {
        // Una PC de escritorio con la red sin identificar. Frenar por algo que
        // no se pudo medir dejaría al sync sin correr nunca y sin decir por qué.
        assert_eq!(hold(&Conditions::default(), &Limits::default(), true), None);
    }

    #[test]
    fn la_red_medida_frena_el_server_pero_no_la_red_local() {
        let c = cond(Some(true), None, None);
        let l = Limits::default();
        assert_eq!(hold(&c, &l, true), Some(Hold::Metered));
        assert_eq!(
            hold(&c, &l, false),
            None,
            "mover un archivo dentro de la misma red no gasta datos"
        );
    }

    #[test]
    fn con_el_permiso_puesto_la_red_medida_no_frena() {
        let c = cond(Some(true), None, None);
        let l = Limits { on_metered: true, ..Limits::default() };
        assert_eq!(hold(&c, &l, true), None);
    }

    #[test]
    fn poca_bateria_frena_todo_no_solo_el_server() {
        let c = cond(Some(false), Some(9), Some(false));
        let l = Limits::default();
        assert_eq!(hold(&c, &l, true), Some(Hold::Battery));
        assert_eq!(hold(&c, &l, false), Some(Hold::Battery));
    }

    #[test]
    fn enchufado_no_importa_cuanta_bateria_queda() {
        let c = cond(Some(false), Some(3), Some(true));
        assert_eq!(hold(&c, &Limits::default(), true), None);
    }

    #[test]
    fn en_cero_el_limite_de_bateria_esta_apagado() {
        let c = cond(Some(false), Some(1), Some(false));
        let l = Limits { min_battery: 0, ..Limits::default() };
        assert_eq!(hold(&c, &l, true), None);
    }

    #[test]
    fn la_bateria_gana_cuando_las_dos_cosas_aplican() {
        // El motivo que se muestra tiene que ser el más urgente: quedarse sin
        // batería a la mitad de una transferencia es peor que gastar datos.
        let c = cond(Some(true), Some(5), Some(false));
        assert_eq!(hold(&c, &Limits::default(), true), Some(Hold::Battery));
    }
}
