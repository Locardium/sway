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

/// Cada cuánto se comprueba que los peers sigan alcanzables. Un connect a la
/// LAN es barato; esto es lo que hace que el gris aparezca en segundos y no
/// cuando venza el TTL de mDNS.
const PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1200);
/// Cada cuántos sondeos se vuelve a consultar la red (6 × 10 s = 1 minuto).
const REBROWSE_EVERY: u32 = 6;

// Nota: no hay vencimiento por tiempo de este lado. Lo maneja mdns-sd, que
// refresca los registros antes de que expiren y emite `ServiceRemoved` cuando
// uno vence de verdad. Un filtro propio por "hace cuánto lo vi" es un bug
// esperando: `ServiceResolved` llega cuando algo CAMBIA, no en cada refresco,
// asi que un peer presente y estable deja de emitir eventos y desapareceria
// de la lista estando ahi.

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
    /// Se resuelve al listar, no al descubrir — si no, parear no se refleja
    /// hasta el proximo arranque.
    pub paired: bool,
    /// Visible ahora mismo en la red. Los pareados que no estan se listan
    /// igual, apagados: siguen siendo tus dispositivos aunque el celu este
    /// sin wifi.
    pub online: bool,
}

#[derive(Default)]
pub struct Peers {
    by_uid: Mutex<HashMap<String, Peer>>,
    /// El evento de baja de mDNS trae el fullname, no el uid.
    fullname_to_uid: Mutex<HashMap<String, String>>,
}

impl Peers {
    /// Peers presentes en la red, ordenados por nombre. Uno sale de aca solo
    /// cuando mDNS avisa que se fue (ver la nota sobre el vencimiento arriba).
    pub fn list(&self) -> Vec<Peer> {
        let mut v: Vec<Peer> = self.by_uid.lock().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        v
    }

    /// Lo que ve la UI: los que estan en la red AHORA, mas los ya pareados
    /// que no estan (apagados, sin wifi, fuera de alcance).
    ///
    /// `paired` sale de la DB en cada llamada y no de lo cacheado en el
    /// descubrimiento: parear no cambia nada de lo que anuncia mDNS, asi que
    /// un flag cacheado ahi se queda viejo hasta que el peer se re-anuncie.
    pub fn merged_list(&self, conn: &rusqlite::Connection) -> Vec<Peer> {
        let mut out = self.list();
        let known = paired_devices(conn);
        for p in out.iter_mut() {
            p.paired = known.iter().any(|(uid, _, _, _)| uid == &p.uid);
        }
        for (uid, name, platform, last_seen) in known {
            if out.iter().any(|p| p.uid == uid) {
                continue;
            }
            out.push(Peer {
                uid,
                name,
                platform,
                proto: PROTO.to_string(),
                addrs: Vec::new(),
                port: 0,
                last_seen,
                paired: true,
                online: false,
            });
        }
        // Primero los que estan en la red; despues alfabetico.
        out.sort_by(|a, b| {
            b.online
                .cmp(&a.online)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        out
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

    /// `(uid, ip, port)` de cada peer con dirección conocida, para sondear.
    fn probe_targets(&self) -> Vec<(String, String, u16)> {
        self.by_uid
            .lock()
            .unwrap()
            .values()
            .filter_map(|p| p.addrs.first().map(|a| (p.uid.clone(), a.clone(), p.port)))
            .collect()
    }

    /// Marca un peer como inalcanzable tras un intento de conexión fallido,
    /// sin esperar al próximo sondeo.
    pub fn mark_unreachable(&self, uid: &str) -> bool {
        self.set_online(uid, false)
    }

    /// Devuelve `true` si el estado cambió (o sea, si hay algo que mostrar).
    fn set_online(&self, uid: &str, online: bool) -> bool {
        let mut map = self.by_uid.lock().unwrap();
        match map.get_mut(uid) {
            Some(p) if p.online != online => {
                p.online = online;
                if online {
                    p.last_seen = crate::db::now_ms();
                }
                true
            }
            _ => false,
        }
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

    spawn_browse_loop(handle, daemon.browse(SERVICE_TYPE)?, uid.to_string());
    Ok(daemon)
}

/// Vuelve a preguntar quién hay en la red, ya.
///
/// Hace falta un disparador manual porque `mdns-sd` **duplica la espera entre
/// consultas** en cada ronda (1 s, 2 s, 4 s… con tope de una hora, RFC 6762
/// §5.2). Es correcto para no inundar la red, pero significa que si se
/// perdieron los primeros paquetes multicast — cosa común en Wi-Fi — el peer
/// aparece recién varios minutos después. Un `browse` nuevo manda la consulta
/// en el acto y reinicia esa cuenta.
///
/// El listener anterior se reemplaza (`service_queriers` está indexado por
/// tipo de servicio), así que su hilo termina solo: no se acumulan.
pub fn refresh(handle: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let (receiver, uid) = {
        let state = handle.state::<crate::AppState>();
        let guard = state.mdns.lock().map_err(|_| "mdns lock")?;
        let daemon = guard.as_ref().ok_or("el descubrimiento no está activo")?;
        let receiver = daemon.browse(SERVICE_TYPE)?;
        let conn = state.db.lock().map_err(|_| "db lock")?;
        (receiver, crate::db::this_device_uid(&conn)?)
    };
    spawn_browse_loop(handle.clone(), receiver, uid);
    Ok(())
}

fn spawn_browse_loop(handle: AppHandle, receiver: mdns_sd::Receiver<ServiceEvent>, me: String) {
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
                        // Los resuelve `merged_list` contra la DB en cada
                        // lectura; cachearlos aca los deja viejos.
                        paired: false,
                        online: true,
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
        log::debug!("[mdns] listener reemplazado por uno nuevo");
    });
}

/// Sondeo de alcanzabilidad: abre un TCP al puerto de sync de cada peer y lo
/// cierra enseguida.
///
/// Hace falta porque mDNS no sirve para saber si alguien SIGUE ahí: el TTL de
/// los registros PTR/TXT es de 4500 s (75 minutos) y `mdns-sd` 0.20 no expone
/// cómo bajarlo, así que un dispositivo que se va sin avisar queda en el
/// cache más de una hora. Peor: "anunciado" y "alcanzable" no son lo mismo —
/// el celular puede seguir en el cache con la app cerrada, y ahí Ping se come
/// el timeout. Un connect es la única respuesta honesta a "¿puedo
/// sincronizar con esto ahora?".
///
/// Del otro lado esto entra al accept loop y corta sin mandar nada: `serve`
/// lo reconoce como sondeo por el EOF y no lo reporta como error.
pub fn spawn_prober(handle: AppHandle) {
    std::thread::spawn(move || {
        let mut tick: u32 = 0;
        loop {
            std::thread::sleep(PROBE_INTERVAL);
            tick += 1;

            // Re-preguntar cada tanto por cuenta propia: sin esto, el backoff
            // de mDNS deja de consultar por hasta una hora, y un dispositivo
            // que se enciende después queda invisible todo ese rato.
            if tick % REBROWSE_EVERY == 0 {
                if let Err(e) = refresh(&handle) {
                    log::debug!("[mdns] re-browse fallo: {e}");
                }
            }

            probe_once(&handle);
        }
    });
}

/// Una ronda de sondeo. Emite `peers-changed` solo si algo cambió de estado.
pub fn probe_once(handle: &AppHandle) {
    let state = handle.state::<crate::AppState>();
    let mut changed = false;
    let mut came_online = Vec::new();
    for (uid, ip, port) in state.peers.probe_targets() {
        let reachable = match format!("{ip}:{port}").parse() {
            Ok(addr) => std::net::TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok(),
            Err(_) => false,
        };
        if state.peers.set_online(&uid, reachable) {
            changed = true;
            if reachable {
                came_online.push(uid);
            }
        }
    }
    if changed {
        let _ = handle.emit("peers-changed", ());
    }
    // Volvió a estar disponible: ponerse al día sin esperar a que cambie algo.
    // El celular puede haber estado sin wifi toda la tarde.
    for uid in came_online {
        let paired = {
            let guard = state.db.lock();
            match guard {
                Ok(conn) => conn
                    .query_row("SELECT 1 FROM devices WHERE uid = ?1", [&uid], |_| Ok(()))
                    .is_ok(),
                Err(_) => false,
            }
        };
        if paired {
            crate::autosync::peer_came_online(handle, &uid);
        }
    }
}

/// `(uid, name, platform, last_seen)` de los dispositivos ya pareados.
fn paired_devices(conn: &rusqlite::Connection) -> Vec<(String, String, String, i64)> {
    let mut stmt = match conn
        .prepare("SELECT uid, name, platform, COALESCE(last_seen, 0) FROM devices")
    {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)));
    match rows {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(_) => Vec::new(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(peers: &Peers, uid: &str, name: &str) {
        peers.upsert(
            format!("{uid}._sway._tcp.local."),
            Peer {
                uid: uid.into(),
                name: name.into(),
                platform: "windows".into(),
                proto: PROTO.into(),
                addrs: vec!["192.168.0.5".into()],
                port: 1234,
                last_seen: crate::db::now_ms(),
                paired: false,
                online: true,
            },
        );
    }

    fn db_with_device(uid: &str, name: &str) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO devices (uid, name, platform, paired_at, last_seen)
             VALUES (?1, ?2, 'android', 1, 2)",
            rusqlite::params![uid, name],
        )
        .unwrap();
        conn
    }

    /// El flag `paired` tiene que salir de la DB en cada lectura. Cuando se
    /// cacheaba al descubrir, parear no se reflejaba hasta reiniciar la app:
    /// mDNS no vuelve a anunciar nada cuando se guarda un dispositivo.
    #[test]
    fn pairing_shows_up_without_a_new_mdns_announcement() {
        let peers = Peers::default();
        seen(&peers, "peer-1", "Celu");
        let empty = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::init_schema(&empty).unwrap();
        assert!(!peers.merged_list(&empty)[0].paired);

        // Mismo estado de descubrimiento, pero ya pareado.
        let paired = db_with_device("peer-1", "Celu");
        let list = peers.merged_list(&paired);
        assert_eq!(list.len(), 1);
        assert!(list[0].paired);
        assert!(list[0].online);
    }

    #[test]
    fn paired_devices_stay_listed_while_offline() {
        let peers = Peers::default();
        let conn = db_with_device("guardado", "PC del living");
        let list = peers.merged_list(&conn);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "PC del living");
        assert!(list[0].paired);
        assert!(!list[0].online);
        assert!(list[0].addrs.is_empty());
    }

    /// Un peer presente y estable deja de generar eventos `ServiceResolved`
    /// (solo llegan cuando algo cambia), así que su `last_seen` se queda
    /// viejo. Filtrar por eso lo hacía "desaparecer" de la red estando ahí:
    /// quién sigue presente lo decide mdns-sd con `ServiceRemoved`.
    #[test]
    fn a_quiet_peer_stays_online() {
        let peers = Peers::default();
        seen(&peers, "peer-1", "Celu");
        {
            let mut map = peers.by_uid.lock().unwrap();
            map.get_mut("peer-1").unwrap().last_seen = crate::db::now_ms() - 60 * 60 * 1000;
        }
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::init_schema(&conn).unwrap();
        let list = peers.merged_list(&conn);
        assert_eq!(list.len(), 1);
        assert!(list[0].online);
    }

    /// mDNS dice "lo anunciaron", no "sigue ahí": con un TTL de 75 minutos,
    /// un peer que se fue sigue en el cache. El sondeo TCP es lo que decide
    /// si está online, y un intento de conexión fallido tiene que reflejarse
    /// enseguida — quedar "conectado" justo después de un timeout es la peor
    /// combinación posible.
    #[test]
    fn a_failed_connection_marks_the_peer_offline() {
        let peers = Peers::default();
        seen(&peers, "peer-1", "Celu");
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::init_schema(&conn).unwrap();
        assert!(peers.merged_list(&conn)[0].online);

        assert!(peers.mark_unreachable("peer-1"));
        assert!(!peers.merged_list(&conn)[0].online);
        // Segunda vez no cambia nada: no hay por qué re-emitir el evento.
        assert!(!peers.mark_unreachable("peer-1"));

        // Y vuelve solo cuando responde de nuevo.
        assert!(peers.set_online("peer-1", true));
        assert!(peers.merged_list(&conn)[0].online);
    }

    #[test]
    fn removed_peers_disappear() {
        let peers = Peers::default();
        seen(&peers, "peer-1", "Celu");
        assert!(peers.remove_by_fullname("peer-1._sway._tcp.local."));
        assert!(peers.list().is_empty());
    }

    /// Los que están en la red van arriba; el resto, alfabético.
    #[test]
    fn online_peers_sort_first() {
        let peers = Peers::default();
        seen(&peers, "zeta", "Zeta");
        let conn = db_with_device("alfa", "Alfa");
        let names: Vec<String> = peers.merged_list(&conn).into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["Zeta", "Alfa"]);
    }
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
