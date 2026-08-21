package com.sway.app

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.net.wifi.WifiManager
import android.os.Build
import android.provider.Settings
import android.system.Os
import android.os.Bundle
import android.util.Log
import android.webkit.WebView
import androidx.media3.ui.PlayerNotificationManager
import app.tauri.nativeaudio.NativeAudioRuntime

/// Sway maneja la cola de reproduccion del lado JS (ver App.tsx), asi que la
/// app necesita dos cosas del lado nativo que Tauri no da solo.
class MainActivity : TauriActivity() {

    /// El `MediaSession` del plugin sobrevive a la Activity (lo sostiene un
    /// foreground service), y `tauri android dev` la recrea en cada reload.
    /// Por eso el interceptor de transporte no puede quedarse con una
    /// referencia a la Activity: apuntaria a una WebView muerta despues del
    /// primer reload. Resuelve contra la Activity viva en cada toque.
    companion object {
        private const val TAG = "Sway"

        @Volatile
        private var live: MainActivity? = null

        fun dispatchMediaButton(button: String) {
            Log.i(TAG, "boton multimedia: $button")
            eval("window.__swayMediaButton && window.__swayMediaButton('$button')")
        }

        private fun eval(js: String) {
            val activity = live ?: return
            val view = activity.webView ?: return
            view.post { view.evaluateJavascript(js, null) }
        }
    }

    private var webView: WebView? = null

    override fun onWebViewCreate(webView: WebView) {
        this.webView = webView
        // Sway es una app, no una pagina: el pinch y el doble tap no tienen
        // que escalar la UI. El meta viewport ya lo pide, esto lo garantiza
        // aunque el WebView decida ignorarlo.
        webView.settings.setSupportZoom(false)
        webView.settings.builtInZoomControls = false
        webView.settings.displayZoomControls = false
    }

    /// Sin esto, el descubrimiento mDNS del sync (Fase 5.1) no recibe NADA.
    ///
    /// El Wi-Fi de Android descarta los paquetes multicast y broadcast que no
    /// van dirigidos a la interfaz, para ahorrar bateria. mDNS es multicast
    /// puro (224.0.0.251:5353), asi que sin el lock la busqueda corre
    /// perfecta, no da ningun error, y simplemente no aparece jamas un
    /// dispositivo. El anuncio SI sale — o sea que la PC ve al celu pero el
    /// celu no ve a nadie, que es la pista para reconocerlo.
    private var multicastLock: WifiManager.MulticastLock? = null

    private fun acquireMulticastLock() {
        if (multicastLock != null) return
        val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager
        multicastLock = wifi?.createMulticastLock("sway-mdns")?.apply {
            setReferenceCounted(false)
            runCatching { acquire() }
                .onSuccess { Log.i(TAG, "MulticastLock tomado (mDNS puede recibir)") }
                .onFailure { Log.w(TAG, "MulticastLock fallo: $it") }
        }
    }

    /// El nombre del equipo, para que en la lista de sync del otro dispositivo
    /// diga "Galaxy S24+" y no "Android".
    ///
    /// Solo existe del lado de Java, y Rust no puede ir a buscarlo por JNI:
    /// `ndk_context` (la via estandar para conseguir el JavaVM) nunca queda
    /// inicializado en una app Tauri, y su `expect` aborta el proceso. Asi
    /// que se resuelve aca y se pasa por variable de entorno.
    ///
    /// **Tiene que correr ANTES de `super.onCreate()`**: ahi es donde arranca
    /// el runtime de Rust, que la lee en su setup.
    private fun exportDeviceName() {
        val userName = runCatching {
            Settings.Global.getString(contentResolver, "device_name")
        }.getOrNull()?.trim()

        val name = if (!userName.isNullOrEmpty()) {
            userName
        } else {
            val model = Build.MODEL?.trim().orEmpty()
            val maker = Build.MANUFACTURER?.trim().orEmpty()
            // "samsung SM-S926B" queda redundante si el modelo ya nombra la marca.
            if (maker.isNotEmpty() && !model.lowercase().startsWith(maker.lowercase())) {
                "${maker.replaceFirstChar { it.uppercase() }} $model"
            } else {
                model
            }
        }

        if (name.isNotEmpty()) {
            runCatching { Os.setenv("SWAY_DEVICE_NAME", name, true) }
                .onSuccess { Log.i(TAG, "nombre del dispositivo: $name") }
                .onFailure { Log.w(TAG, "no se pudo exportar el nombre: $it") }
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        exportDeviceName()
        super.onCreate(savedInstanceState)
        live = this
        installTransportInterceptor()
        acquireMulticastLock()

        val filter = IntentFilter().apply {
            addAction(PlayerNotificationManager.ACTION_FAST_FORWARD)
            addAction(PlayerNotificationManager.ACTION_REWIND)
            addAction(PlayerNotificationManager.ACTION_NEXT)
            addAction(PlayerNotificationManager.ACTION_PREVIOUS)
        }
        // Se registra por toda la vida de la activity, no por onStart/onStop:
        // la notificacion se usa justamente con la app en background, o sea
        // despues de onStop.
        //
        // Los broadcasts son internos del paquete; desde API 33 hay que
        // declararlo explicitamente o el sistema tira SecurityException.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            registerReceiver(notificationButtons, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            @Suppress("UnspecifiedRegisterReceiverFlag")
            registerReceiver(notificationButtons, filter)
        }
    }

    override fun onDestroy() {
        runCatching { unregisterReceiver(notificationButtons) }
        multicastLock?.let { lock -> runCatching { if (lock.isHeld) lock.release() } }
        multicastLock = null
        if (live === this) live = null
        super.onDestroy()
    }

    /// El WebView sigue vivo con la app en background.
    ///
    /// `WryActivity.onPause()` llama `mWebView.onPause()`, que congela el JS
    /// de la pagina. Para una app comun esta perfecto, pero aca la logica de
    /// reproduccion vive en JS: con el WebView pausado no corre el
    /// auto-advance al terminar un track, no se actualiza la posicion, y no
    /// llegan los botones de la notificacion — todo eso pasa justamente
    /// mientras la app NO esta en pantalla. Volver a resumirlo despues del
    /// super es la unica forma de evitarlo sin tocar el codigo generado.
    ///
    /// El costo es que el WebView sigue consumiendo en background; se asume
    /// porque cuando eso pasa hay audio sonando de todas formas (el plugin
    /// mantiene un foreground service).
    override fun onPause() {
        super.onPause()
        webView?.onResume()
        // Con la app fuera de pantalla, el proceso pierde el derecho a llamar
        // `Service.startForeground()`. El plugin lo llama cada vez que su
        // notificacion vuelve a ser "ongoing" — y sale de foreground cuando el
        // track llega a STATE_ENDED. O sea: terminar un track en background y
        // arrancar el siguiente crasheaba con
        // ForegroundServiceStartNotAllowedException. La app usa esta senal
        // para adelantar el cambio de track y no pisar nunca ese estado (ver
        // nativeAudio.ts).
        eval("window.__swayAppVisible && window.__swayAppVisible(false)")
    }

    override fun onResume() {
        super.onResume()
        eval("window.__swayAppVisible && window.__swayAppVisible(true)")
    }

    /// Anterior/siguiente de la notificacion y el lockscreen -> la app.
    ///
    /// El player del `MediaSession` redirige a proposito
    /// `seekToNext()`/`seekToPrevious()` a `seekForward()`/`seekBack()`:
    /// saltos de 10s dentro del track, nunca cambio de cancion. Desde Android
    /// 13 los controles de la notificacion multimedia los dibuja el sistema a
    /// partir del `MediaSession`, asi que ese player es el unico punto por
    /// donde pasan esos botones.
    ///
    /// Antes esto se resolvia desde aca reemplazando `mediaSession.player` por
    /// un `ForwardingPlayer` propio, esperando en un Runnable a que la sesion
    /// existiera. Dejo de servir cuando el plugin gano crossfade: cada fade
    /// cambia de deck y reconstruye el player de la sesion, y el reemplazo se
    /// perdia sin aviso. El hook vive ahora adentro del fork
    /// (`crates/native-audio`), que lo re-aplica en cada swap.
    ///
    /// Del lado JS lo recibe `window.__swayMediaButton` (ver App.tsx).
    private fun installTransportInterceptor() {
        NativeAudioRuntime.transportInterceptor = { button -> dispatchMediaButton(button) }
    }

    /// Segundo camino para los mismos botones, por las dudas.
    ///
    /// Desde Android 13 los controles de la notificacion multimedia salen del
    /// `MediaSession` (lo agarra el interceptor de arriba), pero en versiones
    /// anteriores — y en las capas de algunos fabricantes — los dibuja el
    /// propio `PlayerNotificationManager`, que despacha cada boton como un
    /// broadcast dentro del paquete con acciones que son constantes publicas.
    /// Escuchar las dos vias cuesta poco; si llegaran a dispararse las dos por
    /// un mismo toque, el handler de JS descarta el duplicado.
    private val notificationButtons = object : BroadcastReceiver() {
        override fun onReceive(context: Context?, intent: Intent?) {
            when (intent?.action) {
                PlayerNotificationManager.ACTION_FAST_FORWARD,
                PlayerNotificationManager.ACTION_NEXT -> dispatchMediaButton("next")
                PlayerNotificationManager.ACTION_REWIND,
                PlayerNotificationManager.ACTION_PREVIOUS -> dispatchMediaButton("prev")
            }
        }
    }
}
