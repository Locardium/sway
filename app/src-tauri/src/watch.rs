//! Enterarse de lo que cambió en otro dispositivo, sin preguntar cada tanto.
//!
//! El sync lo maneja siempre el que llama: el que atiende responde pedidos y
//! no decide nada. Entre dos dispositivos con pantalla eso alcanza, porque el
//! que cambia algo llama al otro y se lo empuja. Contra el server de archivo
//! no: los cambios hechos afuera de casa llegan al server enseguida, pero el
//! server no llama a nadie —y no puede, que es justo el punto de que los
//! dispositivos marquen hacia él y no al revés—, así que la PC no se entera
//! hasta su próxima pasada periódica. Diez minutos, y hasta veinticinco si la
//! red local todavía estaba ocupada.
//!
//! Acá se invierte quién espera sin invertir quién llama: la app abre una
//! conexión contra el server, manda `Watch` y se queda escuchando. El server
//! contesta `Changed` recién cuando su biblioteca se mueve. La conexión sale
//! de adentro hacia afuera, como todas las demás, así que sigue sin hacer
//! falta que nadie sea alcanzable desde internet.
//!
//! **Por qué esperar y no preguntar:** una conexión parada no cuesta nada
//! mientras no pasa nada. Preguntar cada 10 segundos —que es lo que ya hacía
//! el sondeo de alcanzabilidad— son 8640 conexiones por día contra el server,
//! y en un celular cada una despierta la radio. Mientras esta conexión está
//! viva el sondeo saltea a ese dispositivo (ver `discovery::probe_once`): la
//! conexión misma es la prueba de que está alcanzable, y una mejor que un
//! connect, porque se corta en el momento en que deja de estarlo. Lo único que
//! viaja en reposo es un latido cada 45 segundos.
//!
//! **En un celular esta conexión se cae todo el tiempo, y está bien.** Cambia
//! de wifi a datos, la corta el NAT de la operadora, Doze la mata con la
//! pantalla apagada: medido contra el server de casa, vivía entre 17 y 47
//! segundos por vez. Por eso lo que importa no es que dure, sino que
//! **reconectar sea barato**: se manda la última revisión conocida y el otro
//! lado contesta si de verdad nos perdimos algo. Un corte sale un handshake,
//! no un inventario entero. Y por eso también la espera creciente sólo crece
//! cuando no se puede ni conectar — un corte después de un rato conectado no
//! es un server caído, y tratarlo como tal degrada el aviso instantáneo a un
//! sondeo de cinco minutos justo donde más falta hace.

use crate::AppState;
use std::collections::HashSet;
use std::sync::{Condvar, Mutex};
use std::time::Duration;
use sway_core::wire::{Mark, Msg};
use tauri::{AppHandle, Emitter, Manager};

/// Cuánto se espera un latido antes de dar la conexión por muerta.
///
/// Dos latidos y pico (`engine::WATCH_HEARTBEAT` son 45 segundos): perder uno
/// suelto no puede costar una reconexión, pero tampoco se puede pasar un rato
/// largo creyendo que hay alguien del otro lado. Es también el techo de lo que
/// un dispositivo caído puede seguir apareciendo verde, porque mientras dura
/// esta conexión el sondeo no lo toca.
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// Espera después de un corte, y su techo. Se duplica en cada intento
/// fallido: un server apagado no merece un intento cada quince segundos toda
/// la noche.
const RETRY_MIN: Duration = Duration::from_secs(15);
const RETRY_MAX: Duration = Duration::from_secs(5 * 60);

/// Lo que se espera cuando la conexión venía andando y se cortó.
///
/// Corto a propósito, y es lo que se nota al reiniciar el server: la conexión
/// muere en el acto (el cierre llega como un corte, no como un silencio), así
/// que lo único que separa al dispositivo de volver a estar al día es esta
/// espera. Con quince segundos, reiniciar el server se sentía como que
/// "tardaba en darse cuenta".
///
/// No hace falta más: si el corte se repite, la conexión igual vive decenas de
/// segundos entre uno y otro, así que esto no es un bucle de reintentos — es
/// un handshake cada tanto. Y si se cae en el acto, de eso se ocupa el conteo
/// de caídas instantáneas.
const RETRY_AFTER_DROP: Duration = Duration::from_secs(3);

/// Cada cuánto se vuelve a mirar si el sync sigue en pausa (apagado, red
/// medida, poca batería). No es un error: es una decisión que puede cambiar.
const HOLD_RETRY: Duration = Duration::from_secs(60);

/// Cuánto se espera cuando el otro lado no sabe reportar cambios (un server
/// anterior a esto). No se arregla reintentando; sólo se vuelve a probar cada
/// tanto por si lo actualizaron.
const UNSUPPORTED_RETRY: Duration = Duration::from_secs(60 * 60);

/// Una espera que dura menos que esto no fue una espera: la conexión se abrió
/// y se cayó en el acto.
///
/// Un segundo, no cinco. Lo que esto tiene que reconocer es un server que lee
/// el pedido, no lo entiende y corta: eso pasa en menos de lo que tarda una
/// ida y vuelta, no en segundos. Cinco segundos era un umbral prestado del
/// aire, y todo lo que cae adentro sin ser un server viejo —un corte de red
/// justo después de conectar, un handover del celular— se contaba como una
/// prueba en contra del otro lado.
const TOO_QUICK: Duration = Duration::from_secs(1);

/// Cuántas caídas instantáneas seguidas alcanzan para dejar de insistir.
///
/// Un server viejo no contesta `Watch`: acepta la conexión, no entiende el
/// pedido y corta. Eso no se arregla reintentando, así que después de unas
/// cuantas se deja de insistir y se vuelve a la pasada periódica.
///
/// **Sólo cuenta con la sesión ya abierta.** Un server apagado también falla
/// al instante, y ése sí se arregla solo en cuanto vuelva: contarlo acá era
/// dejar de mirar durante una hora cada vez que se reinicia el server, que es
/// justo cuando uno está esperando ver si anda.
///
/// Cinco y no tres porque el precio de los dos errores cambió. Equivocarse
/// para el otro lado cuesta poco: desde que reconectar es un handshake y no un
/// inventario, insistir contra un server viejo es barato. Equivocarse para
/// este lado cuesta una hora de no mirar nada, en silencio — que es la forma
/// de falla que este archivo ya tuvo dos veces.
const TOO_QUICK_LIMIT: u32 = 5;

/// Cada cuánto se revisa si apareció un server nuevo para mirar.
const SUPERVISE: Duration = Duration::from_secs(30);

/// Cada cuánto se compara la biblioteca entera aunque nadie haya avisado nada.
///
/// Los avisos alcanzan mientras funcionen. Esto es para cuando no: un bug en
/// la cuenta de revisiones, un cambio que se aplicó a medias, cualquier cosa
/// que deje las dos bibliotecas distintas sin que nadie se entere. Sin esta
/// pasada, una divergencia así no se arregla nunca — porque el aviso que
/// tendría que dispararla es justamente el que falló.
///
/// Seis horas y no diez minutos porque la comparación no es gratis: es el
/// inventario entero, y con 5000 temas son 4 MB. Cuatro veces por día es una
/// red de contención; cada diez minutos era el gasto principal de la app.
const FULL_CHECK: Duration = Duration::from_secs(6 * 60 * 60);

/// Quién está siendo vigilado, y el timbre para despertarlo.
///
/// `threads` y `live` son dos listas y no una porque contestan preguntas
/// distintas: la primera evita lanzar dos watchers para el mismo dispositivo y
/// lo sigue teniendo mientras reconecta; la segunda es sólo mientras la
/// conexión está realmente abierta, que es lo que le dice al sondeo que no se
/// moleste. Confundirlas dejaría un dispositivo caído pintado de verde durante
/// todo el backoff.
#[derive(Default)]
pub struct Watchers {
    threads: Mutex<HashSet<String>>,
    live: Mutex<HashSet<String>>,
    /// Timbres pendientes, por uid.
    ///
    /// El server no puede avisar que se prendió —no sabe llegar a un
    /// dispositivo detrás de un NAT, y que marquen ellos es justamente lo que
    /// hace que esto ande sin abrir nada—. Pero el sondeo de alcanzabilidad sí
    /// lo ve, cada diez segundos. Sin este timbre esa noticia no le llegaba a
    /// nadie: el watcher seguía durmiendo su espera creciente, hasta cinco
    /// minutos, contra un server que ya estaba levantado.
    ring: Mutex<HashSet<String>>,
    bell: Condvar,
}

impl Watchers {
    /// ¿Hay una conexión de espera abierta contra este dispositivo?
    pub fn is_live(&self, uid: &str) -> bool {
        self.live.lock().map(|l| l.contains(uid)).unwrap_or(false)
    }

    /// ¿Hay un watcher a cargo de este dispositivo, aunque esté esperando?
    pub fn is_watched(&self, uid: &str) -> bool {
        self.threads.lock().map(|t| t.contains(uid)).unwrap_or(false)
    }

    /// "Dejá de esperar, probá ahora." Lo toca quien se entera de algo que el
    /// watcher no puede ver desde donde está: que el server volvió, que
    /// cambiaron las condiciones de red.
    pub fn wake(&self, uid: &str) {
        if let Ok(mut ring) = self.ring.lock() {
            ring.insert(uid.to_string());
        }
        // A todos: cada watcher mira si el timbre era para él.
        self.bell.notify_all();
    }

    /// Espera hasta `max`, o hasta que toquen el timbre para este uid.
    ///
    /// Un timbre que llegó mientras el watcher estaba ocupado no se pierde:
    /// queda anotado y hace que la próxima espera termine en el acto.
    fn nap(&self, uid: &str, max: Duration) {
        let Ok(ring) = self.ring.lock() else {
            std::thread::sleep(max);
            return;
        };
        match self.bell.wait_timeout_while(ring, max, |r| !r.contains(uid)) {
            Ok((mut ring, _)) => {
                ring.remove(uid);
            }
            Err(_) => std::thread::sleep(max),
        }
    }
}

/// Qué hacer después de que una espera terminó mal.
#[derive(Debug, PartialEq)]
enum Next {
    /// Volver a intentar dentro de tanto.
    Retry(Duration),
    /// Dejar de insistir por un rato largo: esto no se arregla reintentando.
    StandDown,
}

/// Cuánto esperar antes del próximo intento, y cuántas veces seguidas se cayó
/// en el acto.
///
/// Vive aparte del bucle para poder probarlo. No es ceremonia: los dos bugs
/// que tuvo esto vivían exactamente acá —la espera que crecía hasta cinco
/// minutos y no volvía nunca, y el server apagado contado como server viejo—
/// y ninguno de los dos se ve leyendo el código. Se ven mirando un log veinte
/// minutos después, cuando ya te comiste el problema.
#[derive(Debug)]
struct Backoff {
    wait: Duration,
    quick_failures: u32,
}

impl Backoff {
    fn new() -> Self {
        Backoff { wait: RETRY_MIN, quick_failures: 0 }
    }

    /// Llegó un aviso: todo lo anterior deja de contar.
    fn news(&mut self) {
        *self = Backoff::new();
    }

    /// No se pudo abrir la sesión. La espera crece, pero no se saca ninguna
    /// conclusión sobre el otro lado: un server apagado vuelve.
    fn unreachable(&mut self) -> Duration {
        let now = self.wait;
        self.wait = (self.wait * 2).min(RETRY_MAX);
        now
    }

    /// La sesión existió y se cortó después de `alive`.
    fn dropped(&mut self, alive: Duration) -> Next {
        if alive >= TOO_QUICK {
            // Se conectó, esperó un rato y recién ahí se cayó: el otro lado
            // está bien, lo que falla es la red de acá —o el server se acaba
            // de reiniciar—. Reintentar rápido: dejar crecer la espera acá
            // convierte el aviso instantáneo en un sondeo de cinco minutos,
            // justo en el dispositivo donde más falta hace y sin que nada lo
            // reporte.
            *self = Backoff::new();
            return Next::Retry(RETRY_AFTER_DROP);
        }
        self.quick_failures += 1;
        if self.quick_failures >= TOO_QUICK_LIMIT {
            *self = Backoff::new();
            return Next::StandDown;
        }
        let now = self.wait;
        self.wait = (self.wait * 2).min(RETRY_MAX);
        Next::Retry(now)
    }
}

/// Qué terminó la espera.
enum Outcome {
    /// El otro lado avisó que hay novedades.
    Changed,
    /// Habla el protocolo pero no sabe avisar.
    Unsupported,
    /// No se pudo ni abrir la sesión: apagado, sin red, reiniciándose.
    ///
    /// Va separado del resto porque desde afuera se ve igual que un server
    /// que corta la conexión apenas la abre —los dos fallan en el acto—, y
    /// significan cosas opuestas: uno se arregla solo en cuanto vuelva, el
    /// otro no se arregla nunca. Confundirlos hacía que reiniciar el server
    /// dejara a los dispositivos sin mirar durante una hora.
    Unreachable,
}

/// Mantiene un watcher por cada server vinculado.
///
/// Es un hilo supervisor y no una lista fija porque los servers se agregan y
/// se sacan con la app abierta: vincular uno tiene que empezar a mirarlo sin
/// reiniciar.
pub fn spawn(handle: AppHandle) {
    std::thread::spawn(move || {
        // Sin server vinculado no hay nada que mirar, y eso se ve igual que
        // "esto no arrancó". Decirlo una vez ahorra buscar el problema del
        // lado equivocado.
        let mut announced = false;
        loop {
            let nuevos = servers_to_watch(&handle);
            if !announced {
                announced = true;
                if nuevos.is_empty() {
                    log::info!("[watch] no server paired: nothing to watch");
                }
            }
            for uid in nuevos {
                let handle = handle.clone();
                std::thread::spawn(move || watcher(handle, uid));
            }
            std::thread::sleep(SUPERVISE);
        }
    });
}

/// Servers vinculados que todavía no tienen watcher. Los marca en el acto:
/// entre listarlos y arrancar el hilo hay una ventana en la que el supervisor
/// podría volver a pasar y lanzar un segundo watcher para el mismo.
fn servers_to_watch(handle: &AppHandle) -> Vec<String> {
    let state = handle.state::<AppState>();
    let uids: Vec<String> = {
        let Ok(conn) = state.db.lock() else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare("SELECT uid FROM devices WHERE platform = ?1") else {
            return Vec::new();
        };
        let rows = stmt.query_map([sway_core::pairing::PLATFORM_SERVER], |r| r.get(0));
        match rows {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    };
    let Ok(mut threads) = state.watchers.threads.lock() else {
        return Vec::new();
    };
    uids.into_iter()
        .filter(|uid| threads.insert(uid.clone()))
        .collect()
}

/// Un watcher: espera novedades, sincroniza cuando le avisan, y vuelta a
/// empezar.
///
/// Vive hasta que el dispositivo deja de estar vinculado. Un corte de red no
/// lo termina —reconecta, que es barato— porque terminar dejaría al server sin
/// nadie mirándolo hasta que el supervisor lo notara.
fn watcher(handle: AppHandle, uid: String) {
    log::info!("[watch] watching {uid} for changes");
    let mut retry = Backoff::new();
    // Hasta dónde sabemos de la biblioteca del otro. Sale de la base, así que
    // sobrevive al cierre de la app: si nada cambió mientras no estábamos, el
    // server lo dice y no se compara ni se transfiere nada. `None` = no
    // sabemos nada de él, y entonces hay que comparar todo.
    let mut since: Option<Mark> = stored_mark(&handle, &uid);
    let mut saved = since;
    // Cuándo fue la última comparación completa.
    let mut last_full = std::time::Instant::now();
    // Último motivo de pausa anunciado. Sin esto habría que elegir entre una
    // línea por minuto o ninguna, y ninguna es peor: un watcher en pausa se ve
    // exactamente igual que uno que nunca arrancó, que es justo lo que uno
    // está tratando de distinguir cuando el sync "no anda".
    let mut paused: Option<String> = None;
    while still_paired(&handle, &uid) {
        if let Some(reason) = on_hold(&handle) {
            if paused.as_deref() != Some(reason.as_str()) {
                log::info!("[watch] not watching {uid} for now: {reason}");
                paused = Some(reason);
            }
            handle.state::<AppState>().watchers.nap(&uid, HOLD_RETRY);
            continue;
        }
        if let Some(before) = paused.take() {
            log::info!("[watch] watching {uid} again ({before} no longer applies)");
        }

        // Ponerse al día ANTES de esperar, pero sólo la primera vez: mientras
        // la app estuvo cerrada pudo cambiar cualquier cosa, y quedarse
        // esperando novedades nuevas no traería nada de eso. De ahí en más
        // alcanza con la referencia — el otro lado sabe si nos perdimos algo y
        // lo contesta en el acto.
        //
        // La diferencia se nota en un celular, donde la conexión no llega al
        // minuto (cambia de wifi a datos, la corta el NAT de la operadora) y
        // se reconecta todo el tiempo. Arrastrar una puesta al día en cada
        // reconexión salía más caro que la pasada periódica que esto venía a
        // mejorar: un inventario entero por internet cada medio minuto.
        // Cada tanto se compara todo aunque nadie haya avisado nada: los
        // avisos alcanzan mientras funcionen, y esto es para cuando no.
        if since.is_some() && last_full.elapsed() >= FULL_CHECK {
            log::info!("[watch] full check with {uid}");
            since = None;
        }
        if since.is_none() {
            catch_up(&handle, &uid);
            last_full = std::time::Instant::now();
        }

        let started = std::time::Instant::now();
        let outcome = wait_for_news(&handle, &uid, &mut since);
        // Antes de mirar cómo terminó: lo que se haya aprendido de la marca
        // vale igual, y sobre todo si terminó mal. Es justo la conexión que se
        // cortó la que no tiene que hacernos empezar de cero la próxima vez.
        if since != saved {
            remember_mark(&handle, &uid, since);
            saved = since;
        }
        match outcome {
            Ok(Outcome::Changed) => {
                log::info!("[watch] {uid} says there are changes");
                retry.news();
                catch_up(&handle, &uid);
            }
            Ok(Outcome::Unsupported) => {
                log::info!("[watch] {uid} does not report changes; leaving it to the periodic sync");
                since = None;
                nap(&handle, &uid, UNSUPPORTED_RETRY);
            }
            // Todavía no está: se espera y se vuelve a probar, sin sacar
            // ninguna conclusión sobre lo que el otro lado sabe hacer. La
            // espera puede ser larga, pero la corta el sondeo en cuanto vea
            // que el server volvió.
            Ok(Outcome::Unreachable) => {
                let d = retry.unreachable();
                nap(&handle, &uid, d);
            }
            Err(e) => {
                log::debug!("[watch] connection with {uid} ended: {e}");
                match retry.dropped(started.elapsed()) {
                    Next::Retry(d) => nap(&handle, &uid, d),
                    Next::StandDown => {
                        log::info!(
                            "[watch] {uid} keeps dropping the connection right away; leaving it to the periodic sync"
                        );
                        // La referencia no sirve de nada después de una hora
                        // sin mirar: al volver hay que ponerse al día igual.
                        since = None;
                        nap(&handle, &uid, UNSUPPORTED_RETRY);
                    }
                }
            }
        }
    }
    log::info!("[watch] {uid} is no longer paired: stopping");
    if let Ok(mut threads) = handle.state::<AppState>().watchers.threads.lock() {
        threads.remove(&uid);
    }
}

/// Una corrida contra el server, esperando a que termine.
///
/// La red local primero, por lo mismo que en `autosync`: bajar por la LAN lo
/// que después el server ya no va a tener que mandar es la diferencia entre un
/// segundo y varios minutos de internet.
fn catch_up(handle: &AppHandle, uid: &str) {
    let lan = crate::autosync::lan_peers(handle);
    crate::pairing::wait_until_idle(handle, &lan, crate::autosync::LAN_FIRST_MAX_WAIT);
    crate::pairing::run_sync_blocking(handle.clone(), uid.to_string(), true);
}

/// Abre la conexión, pide que le avisen, y se queda escuchando.
///
/// `since` entra con la última revisión conocida —con ella el otro lado
/// contesta en el acto si nos perdimos algo, en vez de parkear como si nada
/// hubiera pasado mientras estuvimos desconectados— y **sale actualizada**,
/// incluso si esto termina en error: es lo que hace que la reconexión
/// pregunte por lo último y no por lo de cuando se conectó.
fn wait_for_news(
    handle: &AppHandle,
    uid: &str,
    since: &mut Option<Mark>,
) -> anyhow::Result<Outcome> {
    // No poder conectar no es un error de la espera: es que todavía no hay
    // con quién esperar. Se distingue acá y no más arriba porque es el único
    // punto que sabe si la sesión llegó a existir.
    let mut sess = match crate::pairing::open_session_with(handle, uid, READ_TIMEOUT) {
        Ok(sess) => sess,
        Err(e) => {
            log::debug!("[watch] could not reach {uid}: {e}");
            return Ok(Outcome::Unreachable);
        }
    };
    sess.send(&Msg::Watch { since: *since })?;
    let _live = Live::start(handle, uid);
    loop {
        match sess.recv()? {
            // Sigue vivo y sin novedades. La referencia se mueve igual: si
            // esto se corta después de horas parkeado, la reconexión pregunta
            // por lo último y no por lo de ayer — que en un server con mucho
            // movimiento se contesta con un "puede ser" y un sync de más.
            Msg::Ping { mark } => {
                *since = Some(mark);
                continue;
            }
            Msg::Changed { mark } => {
                *since = Some(mark);
                return Ok(Outcome::Changed);
            }
            Msg::Reject { reason } => {
                log::debug!("[watch] {uid} rejected the watch: {reason}");
                return Ok(Outcome::Unsupported);
            }
            other => return Err(anyhow::anyhow!("unexpected answer to Watch: {other:?}")),
        }
    }
}

/// Mientras existe, este dispositivo cuenta como conectado y el sondeo de
/// alcanzabilidad lo saltea. Se deshace solo cuando la conexión termina, pase
/// lo que pase.
struct Live(AppHandle, String);

impl Live {
    fn start(handle: &AppHandle, uid: &str) -> Self {
        let state = handle.state::<AppState>();
        if let Ok(mut live) = state.watchers.live.lock() {
            live.insert(uid.to_string());
        }
        if state.peers.set_online(uid, true) {
            let _ = handle.emit("peers-changed", ());
        }
        Live(handle.clone(), uid.to_string())
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let state = self.0.state::<AppState>();
        // A variable propia y no como binding del `if let`: ahí sería un
        // temporario que vive más que `state`, y no compila.
        let live = state.watchers.live.lock();
        if let Ok(mut live) = live {
            live.remove(&self.1);
        }
        // Que el dispositivo esté caído no lo decide esto: el corte puede ser
        // de un segundo. El sondeo lo vuelve a tomar apenas sale de la lista y
        // dice la verdad en menos de quince segundos.
    }
}

/// Duerme, pero con el oído puesto: cualquiera que se entere de que este
/// dispositivo volvió a estar al alcance corta la espera.
fn nap(handle: &AppHandle, uid: &str, max: Duration) {
    handle.state::<AppState>().watchers.nap(uid, max);
}

/// La marca guardada de este dispositivo.
fn stored_mark(handle: &AppHandle, uid: &str) -> Option<Mark> {
    let state = handle.state::<AppState>();
    let conn = state.db.lock().ok()?;
    sway_core::pairing::watch_mark(&conn, uid)
}

/// La guarda. Sólo cuando cambió: el latido llega cada 45 segundos y casi
/// siempre trae la misma marca, y no vale tomar el lock de la base —el mismo
/// que necesita la pantalla— para escribir lo que ya estaba.
fn remember_mark(handle: &AppHandle, uid: &str, mark: Option<Mark>) {
    let state = handle.state::<AppState>();
    let Ok(conn) = state.db.lock() else { return };
    if let Err(e) = sway_core::pairing::set_watch_mark(&conn, uid, mark) {
        log::debug!("[watch] could not store the mark of {uid}: {e}");
    }
}

fn still_paired(handle: &AppHandle, uid: &str) -> bool {
    let state = handle.state::<AppState>();
    let Ok(conn) = state.db.lock() else {
        return true; // no se pudo mirar: no es motivo para abandonar
    };
    conn.query_row("SELECT 1 FROM devices WHERE uid = ?1", [uid], |_| Ok(()))
        .is_ok()
}

/// Por qué no corresponde tener una conexión abierta ahora mismo.
///
/// Si el sync automático está apagado, o la red se paga por dato y el usuario
/// pidió no gastarla, que le avisen no sirve de nada: el aviso terminaría en
/// un sync que no se va a hacer.
fn on_hold(handle: &AppHandle) -> Option<String> {
    let state = handle.state::<AppState>();
    let conditions = crate::power::current(&state);
    let conn = state.db.lock().ok()?;
    if !crate::autosync::enabled(&conn) {
        return Some("automatic sync is off".into());
    }
    crate::power::hold(&conditions, &crate::power::Limits::load(&conn), true)
        .map(|h| h.reason().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un server apagado vuelve. La espera crece para no martillarlo, pero
    /// nunca se concluye que el server no sabe avisar: contarlo así dejaba a
    /// los dispositivos sin mirar durante una hora cada vez que se reinicia el
    /// server — o sea justo cuando uno está esperando ver si anda.
    #[test]
    fn un_server_apagado_nunca_se_da_por_perdido() {
        let mut b = Backoff::new();
        for _ in 0..20 {
            let d = b.unreachable();
            assert!(d <= RETRY_MAX);
        }
        assert_eq!(b.wait, RETRY_MAX, "la espera tiene techo");
        assert_eq!(b.quick_failures, 0, "no puede acumular nada");
    }

    /// Una conexión que vivió un rato y se cayó es la red de acá, no un server
    /// caído: se reintenta rápido. Sin esto, un celular que pierde la conexión
    /// cada cuarenta segundos terminaba mirando cada cinco minutos, y el aviso
    /// instantáneo se degradaba solo a un sondeo peor que el tick periódico.
    #[test]
    fn una_conexion_que_vivio_no_hace_crecer_la_espera() {
        let mut b = Backoff::new();
        // Antes de esto, varias caídas dejaban la espera arriba.
        b.unreachable();
        b.unreachable();
        assert!(b.wait > RETRY_MIN);

        assert_eq!(
            b.dropped(Duration::from_secs(45)),
            Next::Retry(RETRY_AFTER_DROP),
            "una conexión que venía andando se reintenta enseguida"
        );
        assert_eq!(b.wait, RETRY_MIN, "una conexión sana resetea la espera");
    }

    /// Una sesión que se abre y se cae en el acto, una y otra vez, es un
    /// server que no entiende el pedido. Eso no se arregla reintentando.
    #[test]
    fn caerse_en_el_acto_varias_veces_es_dejar_de_insistir() {
        let mut b = Backoff::new();
        let instante = Duration::from_millis(20);
        for _ in 0..TOO_QUICK_LIMIT - 1 {
            assert!(matches!(b.dropped(instante), Next::Retry(_)));
        }
        assert_eq!(b.dropped(instante), Next::StandDown);
    }

    /// Y una caída instantánea suelta, entre conexiones sanas, no cuenta para
    /// eso: se vuelve a empezar de cero.
    #[test]
    fn una_caida_instantanea_suelta_no_acumula() {
        let mut b = Backoff::new();
        b.dropped(Duration::from_millis(20));
        b.dropped(Duration::from_secs(45));
        assert_eq!(b.quick_failures, 0);
        // Y desde ahí hacen falta todas de nuevo.
        let instante = Duration::from_millis(20);
        for _ in 0..TOO_QUICK_LIMIT - 1 {
            assert!(matches!(b.dropped(instante), Next::Retry(_)));
        }
        assert_eq!(b.dropped(instante), Next::StandDown);
    }

    /// Sin timbre, la espera dura lo que tiene que durar.
    #[test]
    fn sin_timbre_la_espera_se_cumple() {
        let w = Watchers::default();
        let start = std::time::Instant::now();
        w.nap("srv", Duration::from_millis(120));
        assert!(start.elapsed() >= Duration::from_millis(100));
    }

    /// El sondeo ve que el server volvió y corta la espera. Sin esto, un
    /// server que se prende queda sin nadie mirándolo hasta cinco minutos,
    /// porque el server no puede avisar y el watcher está durmiendo.
    #[test]
    fn el_timbre_corta_la_espera() {
        let w = std::sync::Arc::new(Watchers::default());
        let bg = std::sync::Arc::clone(&w);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            bg.wake("srv");
        });
        let start = std::time::Instant::now();
        w.nap("srv", Duration::from_secs(10));
        assert!(start.elapsed() < Duration::from_secs(5), "durmió el plazo entero");
    }

    /// Un timbre que llega mientras el watcher está ocupado no se pierde: la
    /// próxima espera termina en el acto. Si no, la noticia de que el server
    /// volvió se cae justo cuando llega en el peor momento.
    #[test]
    fn un_timbre_anterior_a_la_espera_no_se_pierde() {
        let w = Watchers::default();
        w.wake("srv");
        let start = std::time::Instant::now();
        w.nap("srv", Duration::from_secs(10));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    /// Y es para uno solo: el timbre de un dispositivo no despierta al watcher
    /// de otro.
    #[test]
    fn el_timbre_es_para_uno_solo() {
        let w = Watchers::default();
        w.wake("otro");
        let start = std::time::Instant::now();
        w.nap("srv", Duration::from_millis(120));
        assert!(start.elapsed() >= Duration::from_millis(100));
    }

    /// Un aviso borra todo lo anterior: la conexión funcionó de punta a punta.
    #[test]
    fn un_aviso_deja_todo_como_al_principio() {
        let mut b = Backoff::new();
        b.unreachable();
        b.dropped(Duration::from_millis(20));
        b.news();
        assert_eq!(b.wait, RETRY_MIN);
        assert_eq!(b.quick_failures, 0);
    }
}
