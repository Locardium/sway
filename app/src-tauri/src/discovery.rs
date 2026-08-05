//! Descubrimiento de dispositivos Sway en la red local (mDNS / Bonjour).
//!
//! Fase 5.1: SOLO descubrir. Publica este dispositivo y escucha a los demas;
//! no conecta, no transfiere y no confia en nadie. El pairing cifrado es 5.2.
//!
//! Lo que se publica en el TXT es lo minimo para poder listar y despues abrir
//! una conexion: quien es (uid estable), como llamarlo, en que plataforma
//! corre y que version de protocolo habla. Nada de la biblioteca y nada
//! sensible: cualquiera en la LAN ve estos registros.
//!
//! **Android:** sin `MulticastLock` no llega NINGUN paquete, y sin error — el
//! Wi-Fi descarta multicast para ahorrar bateria. Se toma en
//! `MainActivity.kt`. Si algun dia "no aparece ningun dispositivo" en el celu
//! pero en la PC si, empezar por ahi.

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};

pub const SERVICE_TYPE: &str = "_sway._tcp.local.";

/// Version del protocolo. Un peer que anuncia otra se lista pero se marca
/// incompatible: mejor no ofrecer sincronizar que fallar a mitad de camino.
pub const PROTO: &str = "1";

/// Un peer que no se vuelve a ver en este tiempo sale de la lista. mDNS avisa
/// cuando alguien se va limpiamente; esto cubre el resto (bateria, modo
/// avion, salir del alcance del Wi-Fi).
const STALE_MS: i64 = 3 * 60 * 1000;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Peer {
    pub uid: String,
    pub name: String,
    pub platform: String,
    pub proto: String,
    pub addrs: Vec<String>,
    pub port: u16,
    pub last_seen: i64,
    /// Ya esta en `devices`: alguna vez se pareo con este dispositivo.
    pub paired: bool,
}

#[derive(Default)]
pub struct Peers {
    by_uid: Mutex<HashMap<String, Peer>>,
    /// El evento de baja de mDNS trae el fullname, no el uid.
    fullname_to_uid: Mutex<HashMap<String, String>>,
}

impl Peers {
    /// Peers vistos hace poco, ordenados por nombre. Los vencidos se filtran
    /// al leer en vez de con un timer: no hace falta un thread mas para algo
    /// que solo importa cuando alguien mira la lista.
    pub fn list(&self) -> Vec<Peer> {
        let now = crate::db::now_ms();
        let mut v: Vec<Peer> = self
            .by_uid
            .lock()
            .unwrap()
            .values()
            .filter(|p| now - p.last_seen < STALE_MS)
            .cloned()
            .collect();
        v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        v
    }

    fn upsert(&self, fullname: String, peer: Peer) -> bool {
        self.fullname_to_uid
            .lock()
            .unwrap()
            .insert(fullname, peer.uid.clone());
        let mut map = self.by_uid.lock().unwrap();
        // "Cambio" = algo que el usuario veria distinto. Un refresh de
        // last_seen solo no vale la pena emitirlo: mDNS repregunta seguido y
        // haria re-renderizar la lista todo el tiempo.
        let changed = match map.get(&peer.uid) {
            Some(old) => {
                old.name != peer.name || old.addrs != peer.addrs || old.port != peer.port
            }
            None => true,
        };
        map.insert(peer.uid.clone(), peer);
        changed
    }

    fn remove_by_fullname(&self, fullname: &str) -> bool {
        let uid = self.fullname_to_uid.lock().unwrap().remove(fullname);
        match uid {
            Some(uid) => self.by_uid.lock().unwrap().remove(&uid).is_some(),
            None => false,
        }
    }
}

/// Arranca el anuncio y la busqueda. El `ServiceDaemon` devuelto hay que
/// mantenerlo vivo: al soltarlo, el servicio deja de publicarse.
pub fn start(
    handle: AppHandle,
    uid: &str,
    name: &str,
    port: u16,
) -> Result<ServiceDaemon, Box<dyn std::error::Error>> {
    let daemon = ServiceDaemon::new()?;

    // El hostname tiene que ser unico en la red, asi que sale del uid y no
    // del nombre que puso el usuario: dos dispositivos llamados "PC" se
    // pisarian el registro.
    let short = uid.split('-').next().unwrap_or(uid).to_string();
    let props: HashMap<String, String> = HashMap::from([
        ("uid".to_string(), uid.to_string()),
        ("name".to_string(), name.to_string()),
        ("platform".to_string(), platform_name()),
        ("proto".to_string(), PROTO.to_string()),
    ]);
    let info = ServiceInfo::new(
        SERVICE_TYPE,
        &short,
        &format!("sway-{short}.local."),
        "",
        port,
        props,
    )?
    // La libreria sigue los cambios de IP sola: en el celu la direccion
    // cambia al saltar de Wi-Fi a datos y volver.
    .enable_addr_auto();
    daemon.register(info)?;
    log::info!("[mdns] publicado {name} ({uid}) en el puerto {port}");

    let receiver = daemon.browse(SERVICE_TYPE)?;
    let me = uid.to_string();
    std::thread::spawn(move || {
        for event in receiver {
            match event {
                ServiceEvent::ServiceResolved(info) => {
                    let txt = &info.txt_properties;
                    let get = |k: &str| txt.get_property_val_str(k).unwrap_or("").to_string();
                    let peer_uid = get("uid");
                    // Nos vemos a nosotros mismos por multicast loopback.
                    if peer_uid.is_empty() || peer_uid == me {
                        continue;
                    }
                    // Las link-local (169.254.x de adaptadores virtuales,
                    // fe80::) van al final: son direcciones reales pero
                    // inservibles para conectarse, y en esta maquina son las
                    // que ganaban por orden alfabetico. La primera de la
                    // lista es la que muestra la UI y la que va a usar 5.2.
                    let mut addrs: Vec<(bool, String)> = info
                        .addresses
                        .iter()
                        .filter(|a| !a.is_loopback())
                        .map(|a| (is_link_local(a), a.to_string()))
                        .collect();
                    addrs.sort();
                    let addrs: Vec<String> = addrs.into_iter().map(|(_, a)| a).collect();
                    let name = {
                        let n = get("name");
                        if n.is_empty() { peer_uid.clone() } else { n }
                    };
                    let peer = Peer {
                        uid: peer_uid.clone(),
                        name,
                        platform: get("platform"),
                        proto: get("proto"),
                        addrs,
                        port: info.port,
                        last_seen: crate::db::now_ms(),
                        paired: is_paired(&handle, &peer_uid),
                    };
                    let state = handle.state::<crate::AppState>();
                    if state.peers.upsert(info.fullname.clone(), peer) {
                        log::info!("[mdns] peer visible: {}", info.fullname);
                        let _ = handle.emit("peers-changed", ());
                    }
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    let state = handle.state::<crate::AppState>();
                    if state.peers.remove_by_fullname(&fullname) {
                        log::info!("[mdns] peer se fue: {fullname}");
                        let _ = handle.emit("peers-changed", ());
                    }
                }
                _ => {}
            }
        }
        log::info!("[mdns] busqueda terminada");
    });
    Ok(daemon)
}

/// Direcciones autoasignadas cuando no hay DHCP (`169.254.0.0/16`, `fe80::/10`).
/// Aparecen en adaptadores virtuales (WSL, VirtualBox, Hyper-V) y no sirven
/// para alcanzar al peer.
fn is_link_local(ip: &mdns_sd::ScopedIp) -> bool {
    match ip.to_ip_addr() {
        std::net::IpAddr::V4(v4) => v4.is_link_local(),
        std::net::IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

fn is_paired(handle: &AppHandle, uid: &str) -> bool {
    let state = handle.state::<crate::AppState>();
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(_) => return false,
    };
    conn.query_row("SELECT 1 FROM devices WHERE uid = ?1", [uid], |_| Ok(()))
        .is_ok()
}

fn platform_name() -> String {
    if cfg!(target_os = "android") {
        "android".into()
    } else if cfg!(target_os = "windows") {
        "windows".into()
    } else if cfg!(target_os = "macos") {
        "macos".into()
    } else {
        "linux".into()
    }
}
