package app.tauri.nativeaudio

import android.Manifest
import android.app.Activity
import android.app.PendingIntent
import android.content.Context
import android.content.SharedPreferences
import android.content.Intent
import android.content.pm.PackageManager
import android.media.AudioDeviceInfo
import android.media.AudioManager
import android.media.audiofx.LoudnessEnhancer
import android.net.Uri
import android.app.ActivityManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.util.Log
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import androidx.media3.common.AudioAttributes
import androidx.media3.common.C
import androidx.media3.common.ForwardingPlayer
import androidx.media3.common.MediaItem
import androidx.media3.common.MediaMetadata
import androidx.media3.common.PlaybackException
import androidx.media3.common.Player
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.session.MediaSession
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSArray
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import kotlin.math.PI
import kotlin.math.cos
import kotlin.math.max
import kotlin.math.pow
import kotlin.math.sin

private const val TAG = "plugin/native-audio"
private const val EVENT_STATE = "native_audio_state"
private const val NOTIFICATION_PERMISSION_REQUEST_CODE = 9512
private const val FOREGROUND_PROGRESS_TICK_MS = 25L
private const val BACKGROUND_PROGRESS_TICK_MS = 250L
private const val SEEK_INCREMENT_MS = 10_000L
private const val SEEK_STATE_STALE_MS = 1_500L
private const val PROGRESS_PERSIST_THROTTLE_MS = 1_000L
private const val PROGRESS_NEAR_START_EPSILON_SEC = 0.25
private const val PROGRESS_PERSIST_EPSILON_SEC = 0.05
private const val PROGRESS_PREFS_NAME = "tauri_native_audio_progress"
private const val PROGRESS_KEY_STORY_ID = "story_id"
private const val PROGRESS_KEY_CURRENT_TIME = "current_time"
private const val PROGRESS_KEY_UPDATED_AT_MS = "updated_at_ms"
private const val PROGRESS_KEY_STATUS = "status"

/// How often the crossfade ramp is recomputed. 30ms is ~33 steps per second,
/// under the threshold where a volume ramp starts sounding like steps, and
/// cheap enough that it doesn't matter that it runs on the main looper.
private const val FADE_TICK_MS = 30L

/// Longest crossfade accepted. Matches the app's slider; past this the overlap
/// stops being a transition and starts being two tracks playing at once.
private const val MAX_CROSSFADE_SEC = 12.0

/// How far a track may be turned up, in dB. Mirrors `db::MAX_GAIN_DB` on the
/// Rust side, which clamps to the same figure before the value ever gets here.
///
/// Boost cannot go through `ExoPlayer.volume` — that saturates at 1.0 — so it
/// goes through a `LoudnessEnhancer` on the deck's audio session instead. See
/// `applyVolumeLocked`.
private const val MAX_GAIN_DB = 12.0

/// How far a track may be turned down. Deliberately further from zero than the
/// boost ceiling, matching `db::MAX_CUT_DB`: attenuation cannot clip, and a
/// loud modern master needs more room coming down than a quiet one needs
/// going up.
private const val MAX_CUT_DB = 24.0

data class NativeAudioState(
    val status: String,
    val currentTime: Double,
    val duration: Double,
    val isPlaying: Boolean,
    val buffering: Boolean,
    val rate: Double,
    /// Master volume, 0..1. Separate from `gainDb`, which is per track.
    val volume: Double,
    /// Gain of the item playing right now, in dB (ReplayGain/R128).
    val gainDb: Double,
    /// `id` of the item playing right now. This is what tells the frontend a
    /// gapless or crossfaded transition happened: between two queued items
    /// there is no `ended`, only this changing.
    val trackId: Long?,
    /// `id` of what `setNextSource` staged, `null` if nothing is staged.
    val nextTrackId: Long?,
    val crossfade: Double,
    val outputDeviceId: Int?,
    val error: String? = null,
)

data class NativeAudioProgressCheckpoint(
    val id: Long,
    val currentTime: Double,
    val updatedAtMs: Long,
    val status: String? = null,
)

data class NativeAudioOutputDevice(
    val id: Int,
    val type: String,
    val name: String,
)

@InvokeArg
class SetSourceArgs {
    var src: String? = null
    var id: Long? = null
    var title: String? = null
    var artist: String? = null
    var artworkUrl: String? = null
    var gainDb: Double? = null
}

@InvokeArg
class SeekToArgs {
    var position: Double? = null
}

@InvokeArg
class SetRateArgs {
    var rate: Double? = null
}

@InvokeArg
class SetVolumeArgs {
    var volume: Double? = null
}

@InvokeArg
class SetSourceGainArgs {
    var gainDb: Double? = null
}

@InvokeArg
class SetCrossfadeArgs {
    var seconds: Double? = null
}

@InvokeArg
class SetOutputDeviceArgs {
    var id: Int? = null
}

private data class PendingSeekState(
    val shouldResume: Boolean,
    val startedAtMs: Long,
)

/// What `setSource`/`setNextSource` received, kept whole so a staged item can
/// be re-prepared on the other deck when the crossfade setting changes.
private data class SourceSpec(
    val src: String,
    val id: Long?,
    val title: String?,
    val artist: String?,
    val artworkUrl: String?,
    val gainDb: Double,
)

/// Travels inside the `MediaItem` so the id and the gain survive a gapless
/// transition: with a playlist, the item that starts playing was queued a
/// whole track ago and there is nowhere else to read them from.
private data class ItemInfo(val id: Long?, val gainDb: Double)

/// One ExoPlayer plus the ramp factor applied on top of its item's volume.
/// There are at most two: see `startFadeLocked`.
private class Deck(val player: ExoPlayer) {
    var fade: Float = 1f
    /// Gain of what this deck is playing, in dB. Kept here and not read off the
    /// item's tag on every use because `setSourceGain` has to be able to change
    /// it without touching the `MediaItem`: replacing the item changes its
    /// `localConfiguration`, and ExoPlayer reloads the media when that happens
    /// — i.e. the track would restart.
    var gainDb: Double = 0.0
    var listener: Player.Listener? = null

    /// Turns a track UP. `ExoPlayer.volume` saturates at 1.0, so it can only
    /// ever attenuate — a positive gain needs an effect on the audio session.
    ///
    /// Tied to the session id it was built for: ExoPlayer hands out a new one
    /// when the audio track is rebuilt (a format change does it), and an
    /// enhancer left on the old session boosts nothing.
    var boost: LoudnessEnhancer? = null
    var boostSessionId: Int = C.AUDIO_SESSION_ID_UNSET
    /// What was last pushed to the effect, in millibels. The crossfade ramp
    /// re-applies the volume ~33 times a second and almost none of those
    /// change the boost; this keeps the native calls down to the ones that do.
    var boostAppliedMb: Int = 0

    fun releaseBoost() {
        runCatching { boost?.release() }
        boost = null
        boostSessionId = C.AUDIO_SESSION_ID_UNSET
        boostAppliedMb = 0
    }
}

object NativeAudioRuntime {
    private val lock = Any()
    private val tickHandler = Handler(Looper.getMainLooper())
    private var tickScheduled = false

    /// Index 0 is built on `ensure`; index 1 only when a crossfade needs it.
    private val decks = arrayOfNulls<Deck>(2)
    private var activeIdx = 0

    /// The deck that is playing right now, which during a crossfade is already
    /// the incoming one — everything the frontend is told refers to it.
    private val player: ExoPlayer?
        get() = decks[activeIdx]?.player

    private var appContext: Context? = null
    private var mediaSession: MediaSession? = null
    private var mediaSessionPlayer: Player? = null
    private var lastError: String? = null
    private var pendingSeekState: PendingSeekState? = null
    private var currentStoryId: Long? = null
    private var lastProgressPersistedAtMs = 0L
    private var lastProgressPersistedStoryId: Long? = null
    private var lastProgressPersistedTimeSec: Double? = null

    private var masterVolume: Float = 1f
    private var crossfadeMs: Long = 0L
    private var outputDeviceId: Int? = null

    /// What plays after the current item and hasn't started yet.
    private var stagedNext: SourceSpec? = null
    /// Staged while a crossfade is running: the idle deck is busy fading out,
    /// so the request waits for `finishFadeLocked` instead of clobbering it.
    private var pendingNext: SourceSpec? = null

    private var fading = false
    private var fadeStartedAtMs = 0L
    private var fadeOutIdx = -1

    /// Set by the host app to take over next/previous from the notification and
    /// the lock screen; `null` keeps upstream's behaviour of turning them into
    /// 10-second jumps. Re-applied on every deck swap, which is why it lives
    /// here and not in the app's Activity — the wrapper is rebuilt each time
    /// the session changes player and anything installed from outside is lost.
    @Volatile
    var transportInterceptor: ((String) -> Unit)? = null

    /// Notified with the session player whenever the active deck changes, so
    /// the notification can follow it (see `NativeAudioService`).
    private val deckListeners = mutableListOf<(Player) -> Unit>()

    private val tickRunnable = object : Runnable {
        override fun run() {
            val shouldContinue = synchronized(lock) {
                maybeStartCrossfadeLocked()
                val snapshot = snapshotLocked()
                appContext?.let { persistProgressCheckpointLocked(it, snapshot, force = false) }
                NativeAudioPlugin.emitToActive(snapshot)
                val isPlaying = player?.isPlaying == true
                tickScheduled = isPlaying
                isPlaying
            }
            if (shouldContinue) {
                val delay = synchronized(lock) { nextProgressTickDelayLocked() }
                tickHandler.postDelayed(this, delay)
            }
        }
    }

    private val fadeRunnable = object : Runnable {
        override fun run() {
            val again = synchronized(lock) { stepFadeLocked() }
            if (again) {
                tickHandler.postDelayed(this, FADE_TICK_MS)
            } else {
                emitState()
            }
        }
    }

    /// One listener per deck: the callbacks don't say which player fired them,
    /// and during a crossfade the deck that is fading out keeps reporting
    /// position changes and its own `ended` — none of which is what the app is
    /// listening to any more.
    private fun listenerFor(deck: Deck) = object : Player.Listener {
        private fun isActive() = decks[activeIdx] === deck

        /// The audio session is rebuilt whenever the output format changes, and
        /// the boost effect is attached to a session id — so it has to be
        /// re-attached here or a +12 dB track goes quiet again the moment the
        /// sample rate changes under it.
        override fun onAudioSessionIdChanged(audioSessionId: Int) {
            synchronized(lock) { applyVolumeLocked(deck) }
        }

        override fun onPlaybackStateChanged(playbackState: Int) {
            synchronized(lock) {
                if (!isActive()) return@synchronized
                if (playbackState == Player.STATE_ENDED) {
                    appContext?.let { persistProgressCheckpointLocked(it, snapshotLocked(), force = true) }
                }
            }
            if (!synchronized(lock) { isActive() }) return
            syncTicking()
            emitState()
        }

        override fun onIsPlayingChanged(isPlaying: Boolean) {
            if (!synchronized(lock) { isActive() }) return
            syncTicking()
            emitState()
        }

        override fun onPlaybackParametersChanged(playbackParameters: androidx.media3.common.PlaybackParameters) {
            if (!synchronized(lock) { isActive() }) return
            emitState()
        }

        /// A gapless transition: the playlist moved on to the item that
        /// `setNextSource` appended. Nothing was asked for and nothing ended —
        /// the id and the gain have to be picked up from the new item.
        override fun onMediaItemTransition(mediaItem: MediaItem?, reason: Int) {
            synchronized(lock) {
                if (!isActive()) return@synchronized
                val info = itemInfoOf(mediaItem)
                currentStoryId = info?.id?.takeIf { it > 0 }
                deck.gainDb = info?.gainDb ?: 0.0
                stagedNext = null
                applyVolumeLocked(deck)
            }
            // Everything before the current index has already played and is
            // dropped so the playlist never grows past "current + next". Posted
            // instead of done right here: changing the playlist from inside a
            // player callback re-enters ExoPlayer while it is still dispatching.
            tickHandler.post {
                synchronized(lock) {
                    if (decks[activeIdx] !== deck) return@synchronized
                    val exoPlayer = deck.player
                    if (exoPlayer.currentMediaItemIndex > 0) {
                        exoPlayer.removeMediaItems(0, exoPlayer.currentMediaItemIndex)
                    }
                }
            }
            if (!synchronized(lock) { isActive() }) return
            emitState()
        }

        override fun onPositionDiscontinuity(
            oldPosition: Player.PositionInfo,
            newPosition: Player.PositionInfo,
            reason: Int,
        ) {
            if (reason == Player.DISCONTINUITY_REASON_SEEK || reason == Player.DISCONTINUITY_REASON_SEEK_ADJUSTMENT) {
                synchronized(lock) {
                    if (!isActive()) return@synchronized
                    val exoPlayer = deck.player
                    val pendingSeek = pendingSeekState
                    val shouldResume = pendingSeek?.shouldResume ?: exoPlayer.playWhenReady
                    if (!shouldResume && exoPlayer.playWhenReady) exoPlayer.pause()
                    val shouldRecoverPlayback =
                        shouldResume &&
                            !exoPlayer.isPlaying &&
                            exoPlayer.playbackState == Player.STATE_READY &&
                            lastError == null
                    if (shouldRecoverPlayback) exoPlayer.play()
                    appContext?.let { persistProgressCheckpointLocked(it, snapshotLocked(), force = true) }
                }
            }
            if (!synchronized(lock) { isActive() }) return
            syncTicking()
            emitState()
        }

        override fun onPlayerError(error: PlaybackException) {
            Log.e(TAG, "onPlayerError code=${error.errorCodeName} message=${error.message}", error)
            val active = synchronized(lock) {
                val active = isActive()
                if (active) {
                    lastError = error.message ?: "unknown"
                    pendingSeekState = null
                } else {
                    // The staged item failed to prepare. Dropping it is enough:
                    // the current track keeps playing and the app is told there
                    // is no next any more, so it stages another one.
                    stagedNext = null
                    pendingNext = null
                    deck.player.clearMediaItems()
                }
                active
            }
            if (active) syncTicking()
            emitState()
        }
    }

    fun ensure(context: Context) {
        synchronized(lock) {
            if (decks[activeIdx] != null && mediaSession != null) return

            val ctx = context.applicationContext
            appContext = ctx

            val deck = decks[activeIdx] ?: newDeckLocked(ctx).also { decks[activeIdx] = it }

            val launchIntent = ctx.packageManager.getLaunchIntentForPackage(ctx.packageName)
            val pendingIntent = launchIntent?.let {
                val flags = PendingIntent.FLAG_UPDATE_CURRENT or
                    (if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) PendingIntent.FLAG_IMMUTABLE else 0)
                PendingIntent.getActivity(ctx, 0, it, flags)
            }

            if (mediaSession == null) {
                val sessionPlayer = wrapForSession(deck.player)
                mediaSessionPlayer = sessionPlayer
                mediaSession = MediaSession.Builder(ctx, sessionPlayer)
                    .apply {
                        if (pendingIntent != null) setSessionActivity(pendingIntent)
                    }
                    .build()
            }

            lastError = null
            syncTickingLocked()
        }
    }

    /// Both decks are built on the main looper on purpose. ExoPlayer refuses to
    /// be touched from a thread other than the one it was created on, and
    /// `MediaSession` requires its player to be on the application looper — the
    /// second deck is created much later than the first, from whatever thread
    /// asked for a crossfade, so leaving it implicit would be a coin flip.
    private fun newDeckLocked(ctx: Context): Deck {
        val audioAttributes = AudioAttributes.Builder()
            .setUsage(C.USAGE_MEDIA)
            .setContentType(C.AUDIO_CONTENT_TYPE_MUSIC)
            .build()

        val exoPlayer = ExoPlayer.Builder(ctx)
            .setSeekBackIncrementMs(SEEK_INCREMENT_MS)
            .setSeekForwardIncrementMs(SEEK_INCREMENT_MS)
            .setLooper(Looper.getMainLooper())
            .build()
        exoPlayer.setAudioAttributes(audioAttributes, true)
        exoPlayer.setHandleAudioBecomingNoisy(true)
        exoPlayer.setWakeMode(C.WAKE_MODE_LOCAL)

        val deck = Deck(exoPlayer)
        val listener = listenerFor(deck)
        deck.listener = listener
        exoPlayer.addListener(listener)
        applyOutputDeviceLocked(deck)
        applyVolumeLocked(deck)
        return deck
    }

    private fun ensureIdleDeckLocked(): Deck? {
        val ctx = appContext ?: return null
        val idle = 1 - activeIdx
        decks[idle]?.let { return it }
        val deck = newDeckLocked(ctx)
        deck.player.setPlaybackSpeed(player?.playbackParameters?.speed ?: 1f)
        decks[idle] = deck
        return deck
    }

    fun initialize(context: Context) {
        ensure(context)
        emitState()
    }

    fun startService(context: Context) {
        val serviceIntent = Intent(context.applicationContext, NativeAudioService::class.java)
        runCatching {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.applicationContext.startForegroundService(serviceIntent)
            } else {
                context.applicationContext.startService(serviceIntent)
            }
        }.onFailure { error ->
            Log.w(TAG, "startService failed", error)
        }
    }

    fun stopService(context: Context) {
        val serviceIntent = Intent(context.applicationContext, NativeAudioService::class.java)
        context.applicationContext.stopService(serviceIntent)
    }

    fun setSource(
        context: Context,
        src: String,
        storyId: Long?,
        title: String?,
        artist: String?,
        artworkUrl: String?,
        gainDb: Double?,
    ) {
        synchronized(lock) {
            ensure(context)
            val deck = decks[activeIdx] ?: return
            val exoPlayer = deck.player

            val spec = SourceSpec(src, storyId, title, artist, artworkUrl, safeGain(gainDb))

            // An explicit source cancels anything queued behind it, including a
            // crossfade halfway through.
            abortFadeLocked()
            clearNextLocked()
            pendingSeekState = null
            currentStoryId = storyId?.takeIf { it > 0 }
            deck.gainDb = spec.gainDb
            exoPlayer.setMediaItem(buildMediaItem(spec))
            exoPlayer.prepare()
            applyVolumeLocked(deck)
            lastError = null
            syncTickingLocked()
        }
        emitState()
    }

    /// The gain of the item that is already playing. ReplayGain often arrives
    /// after playback started (the tag is read on another thread, or the value
    /// is computed later), and re-issuing `setSource` to carry it would restart
    /// the track.
    fun setSourceGain(context: Context, gainDb: Double?) {
        synchronized(lock) {
            ensure(context)
            val deck = decks[activeIdx] ?: return
            deck.gainDb = safeGain(gainDb)
            applyVolumeLocked(deck)
        }
        emitState()
    }

    /// What plays after the current item, or `null` to clear it.
    ///
    /// With crossfade off this is appended to the same player's playlist, which
    /// is where ExoPlayer's gapless transition comes from. With crossfade on it
    /// is prepared on the idle deck instead and started early, overlapping.
    fun setNextSource(
        context: Context,
        src: String?,
        storyId: Long?,
        title: String?,
        artist: String?,
        artworkUrl: String?,
        gainDb: Double?,
    ) {
        synchronized(lock) {
            ensure(context)
            if (src.isNullOrBlank()) {
                clearNextLocked()
            } else {
                val spec = SourceSpec(src, storyId, title, artist, artworkUrl, safeGain(gainDb))
                if (fading) pendingNext = spec else stageNextLocked(spec)
            }
        }
        emitState()
    }

    /// Start what is staged right now instead of waiting for the current item
    /// to reach its crossfade point. With crossfade off this is the playlist's
    /// own "next"; with crossfade on it runs the same ramp, just early.
    fun skipToNext(context: Context): Boolean {
        val moved = synchronized(lock) {
            ensure(context)
            val deck = decks[activeIdx] ?: return@synchronized false
            if (crossfadeMs > 0L) {
                if (!fading && stagedNext != null && decks[1 - activeIdx]?.player?.currentMediaItem != null) {
                    startFadeLocked()
                    true
                } else {
                    false
                }
            } else if (deck.player.hasNextMediaItem()) {
                deck.player.seekToNextMediaItem()
                true
            } else {
                false
            }
        }
        emitState()
        return moved
    }

    fun play(context: Context) {
        startService(context)
        synchronized(lock) {
            ensure(context)
            val exoPlayer = player ?: return
            if (exoPlayer.playbackState == Player.STATE_ENDED) {
                exoPlayer.seekTo(0L)
            }
            pendingSeekState = null
            exoPlayer.playWhenReady = true
            exoPlayer.play()
            lastError = null
            syncTickingLocked()
        }
        emitState()
    }

    fun pause(context: Context) {
        synchronized(lock) {
            ensure(context)
            pendingSeekState = null
            // A crossfade is an overlap in wall-clock time: freezing one side of
            // it while the ramp keeps running would resume out of step, so
            // pausing collapses it onto the incoming track first.
            abortFadeLocked()
            player?.pause()
            syncTickingLocked()
            persistProgressCheckpointLocked(context.applicationContext, snapshotLocked(), force = true)
        }
        emitState()
    }

    fun seekTo(context: Context, positionSec: Double) {
        if (!positionSec.isFinite()) return
        synchronized(lock) {
            ensure(context)
            val safeMs = max(0L, (positionSec * 1000.0).toLong())
            // Seeking out of the crossfade window means the transition that was
            // in flight no longer makes sense.
            abortFadeLocked()
            val exoPlayer = player ?: return@synchronized
            val shouldResume = exoPlayer.playWhenReady || exoPlayer.isPlaying
            pendingSeekState = PendingSeekState(shouldResume = shouldResume, startedAtMs = System.currentTimeMillis())
            if (!shouldResume && exoPlayer.playWhenReady) exoPlayer.pause()
            exoPlayer.seekTo(safeMs)
        }
        emitState()
    }

    fun setRate(context: Context, rate: Double) {
        if (!rate.isFinite() || rate <= 0.0) return
        synchronized(lock) {
            ensure(context)
            // Both decks, so a staged track doesn't come in at a different speed.
            decks.forEach { it?.player?.setPlaybackSpeed(rate.toFloat()) }
        }
        emitState()
    }

    /// Master volume, 0..1. Multiplied by each item's own gain.
    fun setVolume(context: Context, volume: Double) {
        if (!volume.isFinite()) return
        synchronized(lock) {
            ensure(context)
            masterVolume = volume.toFloat().coerceIn(0f, 1f)
            decks.forEach { applyVolumeLocked(it) }
        }
        emitState()
    }

    /// Overlap in seconds between one track and the next; `0` turns it off,
    /// which is what puts the staged item back in the same player's playlist
    /// and makes the transition gapless.
    fun setCrossfade(context: Context, seconds: Double) {
        if (!seconds.isFinite()) return
        synchronized(lock) {
            ensure(context)
            val clamped = seconds.coerceIn(0.0, MAX_CROSSFADE_SEC)
            val next = (clamped * 1000.0).toLong()
            if (next == crossfadeMs) return@synchronized
            crossfadeMs = next
            // Whatever was staged was staged for the old mode: same item, other
            // place. A fade already running is left to finish — cutting it in
            // the middle would be audible.
            if (!fading) {
                val restage = stagedNext
                clearNextLocked()
                if (restage != null) stageNextLocked(restage)
            }
        }
        emitState()
    }

    fun listOutputDevices(context: Context): List<NativeAudioOutputDevice> {
        val manager = context.applicationContext.getSystemService(Context.AUDIO_SERVICE) as? AudioManager
            ?: return emptyList()
        return manager.getDevices(AudioManager.GET_DEVICES_OUTPUTS)
            .map {
                NativeAudioOutputDevice(
                    id = it.id,
                    type = deviceTypeName(it.type),
                    name = it.productName?.toString()?.trim().orEmpty().ifBlank { deviceTypeName(it.type) },
                )
            }
    }

    /// `null` hands the choice back to Android. An id that is no longer in the
    /// device list does the same, on purpose: unplugging the headphones the
    /// user had pinned should keep playing on the speaker, not go silent.
    fun setOutputDevice(context: Context, id: Int?) {
        synchronized(lock) {
            ensure(context)
            outputDeviceId = id
            decks.forEach { applyOutputDeviceLocked(it) }
        }
        emitState()
    }

    fun getState(context: Context): NativeAudioState {
        synchronized(lock) {
            ensure(context)
            return snapshotLocked()
        }
    }

    fun getProgressCheckpoint(context: Context): NativeAudioProgressCheckpoint? {
        val prefs = progressPrefs(context.applicationContext)
        val storyId = prefs.getLong(PROGRESS_KEY_STORY_ID, 0L)
        if (storyId <= 0L) return null
        val currentTime = prefs.getFloat(PROGRESS_KEY_CURRENT_TIME, 0f).toDouble()
        val updatedAtMs = prefs.getLong(PROGRESS_KEY_UPDATED_AT_MS, 0L)
        if (!currentTime.isFinite() || currentTime <= 0.0 || updatedAtMs <= 0L) return null
        val status = prefs.getString(PROGRESS_KEY_STATUS, null)
        return NativeAudioProgressCheckpoint(
            id = storyId,
            currentTime = currentTime,
            updatedAtMs = updatedAtMs,
            status = status,
        )
    }

    fun clearProgressCheckpoint(context: Context) {
        synchronized(lock) {
            progressPrefs(context.applicationContext).edit()
                .remove(PROGRESS_KEY_STORY_ID)
                .remove(PROGRESS_KEY_CURRENT_TIME)
                .remove(PROGRESS_KEY_UPDATED_AT_MS)
                .remove(PROGRESS_KEY_STATUS)
                .apply()
            lastProgressPersistedAtMs = 0L
            lastProgressPersistedStoryId = null
            lastProgressPersistedTimeSec = null
        }
    }

    fun dispose(context: Context) {
        synchronized(lock) {
            persistProgressCheckpointLocked(context.applicationContext, snapshotLocked(), force = true)
            tickHandler.removeCallbacks(tickRunnable)
            tickHandler.removeCallbacks(fadeRunnable)
            tickScheduled = false
            fading = false
            fadeOutIdx = -1

            decks.forEachIndexed { i, deck ->
                deck ?: return@forEachIndexed
                deck.listener?.let { deck.player.removeListener(it) }
                deck.releaseBoost()
                deck.player.release()
                decks[i] = null
            }
            activeIdx = 0

            mediaSession?.release()
            mediaSession = null
            mediaSessionPlayer = null

            lastError = null
            pendingSeekState = null
            currentStoryId = null
            stagedNext = null
            pendingNext = null
            appContext = null
        }
        stopService(context)
        emitState()
    }

    fun mediaSession(): MediaSession? {
        synchronized(lock) {
            return mediaSession
        }
    }

    fun mediaSessionPlayer(): Player? {
        synchronized(lock) {
            return mediaSessionPlayer ?: player
        }
    }

    fun addDeckListener(listener: (Player) -> Unit) {
        val current = synchronized(lock) {
            deckListeners.add(listener)
            mediaSessionPlayer ?: player
        }
        current?.let { listener(it) }
    }

    fun removeDeckListener(listener: (Player) -> Unit) {
        synchronized(lock) { deckListeners.remove(listener) }
    }

    // --- Queue and crossfade -------------------------------------------------

    private fun stageNextLocked(spec: SourceSpec) {
        stagedNext = spec
        val item = buildMediaItem(spec)
        if (crossfadeMs > 0L) {
            val idle = ensureIdleDeckLocked() ?: run { stagedNext = null; return }
            idle.fade = 0f
            idle.gainDb = spec.gainDb
            idle.player.setMediaItem(item)
            idle.player.playWhenReady = false
            idle.player.prepare()
            applyVolumeLocked(idle)
            return
        }
        val deck = decks[activeIdx] ?: run { stagedNext = null; return }
        val exoPlayer = deck.player
        val after = exoPlayer.currentMediaItemIndex + 1
        if (exoPlayer.mediaItemCount > after) {
            exoPlayer.removeMediaItems(after, exoPlayer.mediaItemCount)
        }
        exoPlayer.addMediaItem(item)
    }

    private fun clearNextLocked() {
        stagedNext = null
        pendingNext = null
        val idle = decks[1 - activeIdx]
        if (idle != null && !fading) {
            idle.player.pause()
            idle.player.clearMediaItems()
            idle.fade = 1f
        }
        val deck = decks[activeIdx] ?: return
        val exoPlayer = deck.player
        val after = exoPlayer.currentMediaItemIndex + 1
        if (exoPlayer.mediaItemCount > after) {
            exoPlayer.removeMediaItems(after, exoPlayer.mediaItemCount)
        }
    }

    /// Is the current track close enough to its end to start the overlap?
    /// Only reached from the progress tick, which already runs while playing.
    private fun maybeStartCrossfadeLocked() {
        if (fading || crossfadeMs <= 0L || stagedNext == null) return
        val exoPlayer = player ?: return
        if (!exoPlayer.isPlaying) return
        if (decks[1 - activeIdx]?.player?.currentMediaItem == null) return
        val duration = exoPlayer.duration
        if (duration <= 0L) return
        if (duration - exoPlayer.currentPosition > crossfadeMs) return
        startFadeLocked()
    }

    /// The decks swap at the *start* of the ramp, not at the end: from the
    /// moment the incoming track is audible it is the one the app should be
    /// showing, and the frontend needs that early enough to stage the track
    /// after it before the overlap is over.
    private fun startFadeLocked() {
        val inDeck = decks[1 - activeIdx] ?: return
        val outIdx = activeIdx

        inDeck.fade = 0f
        applyVolumeLocked(inDeck)
        inDeck.player.playWhenReady = true
        inDeck.player.play()

        fadeOutIdx = outIdx
        activeIdx = 1 - outIdx
        fading = true
        fadeStartedAtMs = System.currentTimeMillis()
        stagedNext = null
        pendingSeekState = null
        lastError = null
        val info = itemInfoOf(inDeck.player.currentMediaItem)
        currentStoryId = info?.id?.takeIf { it > 0 }
        inDeck.gainDb = info?.gainDb ?: inDeck.gainDb

        bindSessionToActiveLocked()
        tickHandler.removeCallbacks(fadeRunnable)
        tickHandler.post(fadeRunnable)
        syncTickingLocked()
    }

    private fun stepFadeLocked(): Boolean {
        if (!fading) return false
        val total = crossfadeMs.toDouble().coerceAtLeast(1.0)
        val t = ((System.currentTimeMillis() - fadeStartedAtMs) / total).coerceIn(0.0, 1.0)
        // Equal power, not two straight lines: the two tracks are uncorrelated,
        // so they add in power and not in amplitude, and a linear pair dips
        // audibly in the middle.
        decks.getOrNull(fadeOutIdx)?.let {
            it.fade = cos(t * PI / 2.0).toFloat()
            applyVolumeLocked(it)
        }
        decks[activeIdx]?.let {
            it.fade = sin(t * PI / 2.0).toFloat()
            applyVolumeLocked(it)
        }
        if (t < 1.0) return true
        finishFadeLocked()
        return false
    }

    private fun finishFadeLocked() {
        val outDeck = decks.getOrNull(fadeOutIdx)
        fading = false
        fadeOutIdx = -1
        outDeck?.let {
            it.player.pause()
            it.player.clearMediaItems()
            it.fade = 1f
            applyVolumeLocked(it)
        }
        decks[activeIdx]?.let {
            it.fade = 1f
            applyVolumeLocked(it)
        }
        val queued = pendingNext
        pendingNext = null
        if (queued != null) stageNextLocked(queued)
    }

    /// Cuts a crossfade short and leaves only the deck that was coming in —
    /// which is already the active one, so from the app's side nothing changes
    /// except that the outgoing track stops being audible. What was queued
    /// behind the fade is re-staged, not lost.
    private fun abortFadeLocked() {
        tickHandler.removeCallbacks(fadeRunnable)
        if (!fading) return
        decks.getOrNull(fadeOutIdx)?.let {
            it.player.pause()
            it.player.clearMediaItems()
            it.fade = 1f
            applyVolumeLocked(it)
        }
        fading = false
        fadeOutIdx = -1
        decks[activeIdx]?.let {
            it.fade = 1f
            applyVolumeLocked(it)
        }
        val queued = pendingNext
        pendingNext = null
        if (queued != null) stageNextLocked(queued)
    }

    /// Points the `MediaSession` — and with it the notification and the lock
    /// screen — at whichever deck is active now.
    private fun bindSessionToActiveLocked() {
        val exoPlayer = player ?: return
        val wrapper = wrapForSession(exoPlayer)
        mediaSessionPlayer = wrapper
        mediaSession?.player = wrapper
        val listeners = deckListeners.toList()
        tickHandler.post { listeners.forEach { it(wrapper) } }
    }

    private fun wrapForSession(exoPlayer: ExoPlayer): Player = object : ForwardingPlayer(exoPlayer) {
        override fun getAvailableCommands(): Player.Commands {
            return super.getAvailableCommands()
                .buildUpon()
                .add(Player.COMMAND_SEEK_BACK)
                .add(Player.COMMAND_SEEK_FORWARD)
                .add(Player.COMMAND_SEEK_TO_PREVIOUS)
                .add(Player.COMMAND_SEEK_TO_PREVIOUS_MEDIA_ITEM)
                .add(Player.COMMAND_SEEK_TO_NEXT)
                .add(Player.COMMAND_SEEK_TO_NEXT_MEDIA_ITEM)
                .build()
        }

        override fun isCommandAvailable(command: Int): Boolean {
            if (command == Player.COMMAND_SEEK_BACK || command == Player.COMMAND_SEEK_FORWARD) return true
            if (command == Player.COMMAND_SEEK_TO_PREVIOUS || command == Player.COMMAND_SEEK_TO_PREVIOUS_MEDIA_ITEM) return true
            if (command == Player.COMMAND_SEEK_TO_NEXT || command == Player.COMMAND_SEEK_TO_NEXT_MEDIA_ITEM) return true
            return super.isCommandAvailable(command)
        }

        override fun seekToPrevious() = transport("prev") { exoPlayer.seekBack() }
        override fun seekToPreviousMediaItem() = transport("prev") { exoPlayer.seekBack() }
        override fun seekBack() = transport("prev") { exoPlayer.seekBack() }
        override fun seekToNext() = transport("next") { exoPlayer.seekForward() }
        override fun seekToNextMediaItem() = transport("next") { exoPlayer.seekForward() }
        override fun seekForward() = transport("next") { exoPlayer.seekForward() }
    }

    private fun transport(button: String, fallback: () -> Unit) {
        val interceptor = transportInterceptor
        if (interceptor != null) interceptor(button) else fallback()
    }

    // --- Volume and output ---------------------------------------------------

    /// Splits the track's gain across the only two places it can go.
    ///
    /// `ExoPlayer.volume` saturates at 1.0, so it carries everything that
    /// makes the track quieter — the cut half of the gain, the master volume
    /// and the crossfade ramp. Anything above 0 dB cannot be expressed there
    /// at all and is handed to a `LoudnessEnhancer` on the deck's audio
    /// session, which is what makes a +12 dB trim audible instead of silently
    /// doing nothing.
    ///
    /// The boost is dropped while the deck is ducked by the master volume or a
    /// fade: amplifying and then attenuating the same signal only adds the
    /// enhancer's distortion to a level the volume was going to reach anyway.
    private fun applyVolumeLocked(deck: Deck?) {
        val exoPlayer = deck?.player ?: return
        val gain = deck.gainDb.coerceIn(-MAX_CUT_DB, MAX_GAIN_DB)
        val cutDb = gain.coerceAtMost(0.0)
        val boostDb = gain.coerceAtLeast(0.0)
        val duck = masterVolume * deck.fade
        exoPlayer.volume = (duck * dbToLinear(cutDb)).coerceIn(0f, 1f)
        applyBoostLocked(deck, if (duck >= 1f) boostDb else 0.0)
    }

    /// Points the deck's enhancer at whatever audio session it has now and
    /// sets the boost, building or releasing it as needed.
    ///
    /// Every step is guarded: `LoudnessEnhancer` is backed by a device effect
    /// that some ROMs refuse to allocate, and a phone that cannot boost should
    /// keep playing at the level it can reach rather than fall over.
    private fun applyBoostLocked(deck: Deck, boostDb: Double) {
        val sessionId = deck.player.audioSessionId
        if (sessionId == C.AUDIO_SESSION_ID_UNSET) {
            // No session yet — nothing to attach to. Whatever the gain is, it
            // is re-applied once one exists (see onAudioSessionIdChanged).
            deck.releaseBoost()
            return
        }
        // Millibels, and only ever positive: this effect cannot attenuate.
        val targetMb = (boostDb * 100.0).toInt().coerceAtLeast(0)
        if (targetMb == 0) {
            val enhancer = deck.boost ?: return
            if (deck.boostAppliedMb == 0) return
            runCatching { enhancer.enabled = false }
            deck.boostAppliedMb = 0
            return
        }
        if (deck.boost == null || deck.boostSessionId != sessionId) {
            deck.releaseBoost()
            val created = runCatching { LoudnessEnhancer(sessionId) }
                .onFailure { Log.w(TAG, "LoudnessEnhancer unavailable", it) }
                .getOrNull() ?: return
            deck.boost = created
            deck.boostSessionId = sessionId
        }
        val enhancer = deck.boost ?: return
        if (deck.boostAppliedMb == targetMb) return
        runCatching {
            enhancer.setTargetGain(targetMb)
            enhancer.enabled = true
            deck.boostAppliedMb = targetMb
        }.onFailure { Log.w(TAG, "LoudnessEnhancer setTargetGain failed", it) }
    }

    private fun applyOutputDeviceLocked(deck: Deck?) {
        val exoPlayer = deck?.player ?: return
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.M) return
        val wanted = outputDeviceId
        val target: AudioDeviceInfo? = if (wanted == null) {
            null
        } else {
            val manager = appContext?.getSystemService(Context.AUDIO_SERVICE) as? AudioManager
            manager?.getDevices(AudioManager.GET_DEVICES_OUTPUTS)?.firstOrNull { it.id == wanted }
        }
        runCatching { exoPlayer.setPreferredAudioDevice(target) }
            .onFailure { Log.w(TAG, "setPreferredAudioDevice failed", it) }
    }

    private fun dbToLinear(db: Double): Float {
        if (!db.isFinite() || db == 0.0) return 1f
        return 10.0.pow(db / 20.0).toFloat()
    }

    private fun safeGain(gainDb: Double?): Double {
        val v = gainDb ?: return 0.0
        if (!v.isFinite()) return 0.0
        // Same range the Rust side clamps to before sending it, so a value
        // that arrives already clamped passes through untouched.
        return v.coerceIn(-MAX_CUT_DB, MAX_GAIN_DB)
    }

    private fun deviceTypeName(type: Int): String = when (type) {
        AudioDeviceInfo.TYPE_BUILTIN_SPEAKER -> "Speaker"
        AudioDeviceInfo.TYPE_BUILTIN_EARPIECE -> "Earpiece"
        AudioDeviceInfo.TYPE_WIRED_HEADPHONES -> "Wired headphones"
        AudioDeviceInfo.TYPE_WIRED_HEADSET -> "Wired headset"
        AudioDeviceInfo.TYPE_USB_HEADSET -> "USB headset"
        AudioDeviceInfo.TYPE_USB_DEVICE -> "USB audio"
        AudioDeviceInfo.TYPE_USB_ACCESSORY -> "USB accessory"
        AudioDeviceInfo.TYPE_BLUETOOTH_A2DP -> "Bluetooth"
        AudioDeviceInfo.TYPE_BLUETOOTH_SCO -> "Bluetooth (call)"
        AudioDeviceInfo.TYPE_HDMI -> "HDMI"
        AudioDeviceInfo.TYPE_DOCK -> "Dock"
        AudioDeviceInfo.TYPE_AUX_LINE -> "Line out"
        AudioDeviceInfo.TYPE_HEARING_AID -> "Hearing aid"
        else -> "Output $type"
    }

    // --- Ticking and state ---------------------------------------------------

    private fun syncTicking() {
        synchronized(lock) {
            syncTickingLocked()
        }
    }

    private fun syncTickingLocked() {
        val isPlaying = player?.isPlaying == true
        if (isPlaying && !tickScheduled) {
            tickScheduled = true
            tickHandler.removeCallbacks(tickRunnable)
            tickHandler.post(tickRunnable)
            return
        }
        if (!isPlaying && tickScheduled) {
            tickScheduled = false
            tickHandler.removeCallbacks(tickRunnable)
        }
    }

    private fun nextProgressTickDelayLocked(): Long {
        val context = appContext ?: return BACKGROUND_PROGRESS_TICK_MS
        // While the two decks overlap the ramp is what has to stay smooth, and
        // it is driven off its own runnable — but the app also needs the track
        // change reflected right away, so the slow background rate would be
        // wrong here even with the screen off.
        if (fading) return FADE_TICK_MS
        val isForeground = isAppInForeground()
        val isInteractive = isDeviceInteractive(context)
        return if (isForeground && isInteractive) FOREGROUND_PROGRESS_TICK_MS else BACKGROUND_PROGRESS_TICK_MS
    }

    private fun isAppInForeground(): Boolean {
        val processInfo = ActivityManager.RunningAppProcessInfo()
        ActivityManager.getMyMemoryState(processInfo)
        return processInfo.importance == ActivityManager.RunningAppProcessInfo.IMPORTANCE_FOREGROUND ||
            processInfo.importance == ActivityManager.RunningAppProcessInfo.IMPORTANCE_VISIBLE
    }

    private fun isDeviceInteractive(context: Context): Boolean {
        val powerManager = context.getSystemService(Context.POWER_SERVICE) as? PowerManager
        return powerManager?.isInteractive ?: true
    }

    private fun emitState() {
        val snapshot = synchronized(lock) { snapshotLocked() }
        NativeAudioPlugin.emitToActive(snapshot)
    }

    private fun progressPrefs(context: Context): SharedPreferences =
        context.getSharedPreferences(PROGRESS_PREFS_NAME, Context.MODE_PRIVATE)

    private fun persistProgressCheckpointLocked(context: Context, snapshot: NativeAudioState, force: Boolean) {
        val storyId = currentStoryId ?: return
        if (storyId <= 0L) return
        if (!snapshot.currentTime.isFinite() || snapshot.currentTime <= PROGRESS_NEAR_START_EPSILON_SEC) return

        val now = System.currentTimeMillis()
        if (!force && now - lastProgressPersistedAtMs < PROGRESS_PERSIST_THROTTLE_MS) return

        val prevStoryId = lastProgressPersistedStoryId
        val prevTime = lastProgressPersistedTimeSec
        if (!force && prevStoryId == storyId && prevTime != null && kotlin.math.abs(prevTime - snapshot.currentTime) <= PROGRESS_PERSIST_EPSILON_SEC) {
            return
        }

        progressPrefs(context).edit()
            .putLong(PROGRESS_KEY_STORY_ID, storyId)
            .putFloat(PROGRESS_KEY_CURRENT_TIME, snapshot.currentTime.toFloat())
            .putLong(PROGRESS_KEY_UPDATED_AT_MS, now)
            .putString(PROGRESS_KEY_STATUS, snapshot.status)
            .apply()

        lastProgressPersistedAtMs = now
        lastProgressPersistedStoryId = storyId
        lastProgressPersistedTimeSec = snapshot.currentTime
    }

    private fun itemInfoOf(item: MediaItem?): ItemInfo? =
        item?.localConfiguration?.tag as? ItemInfo

    private fun buildMediaItem(spec: SourceSpec): MediaItem {
        val metadataBuilder = MediaMetadata.Builder()
        if (!spec.title.isNullOrBlank()) metadataBuilder.setTitle(spec.title)
        if (!spec.artist.isNullOrBlank()) metadataBuilder.setArtist(spec.artist)
        if (!spec.artworkUrl.isNullOrBlank()) {
            runCatching { Uri.parse(spec.artworkUrl) }
                .onSuccess { metadataBuilder.setArtworkUri(it) }
        }
        return MediaItem.Builder()
            .setUri(spec.src)
            .setTag(ItemInfo(spec.id?.takeIf { it > 0 }, spec.gainDb))
            .setMediaMetadata(metadataBuilder.build())
            .build()
    }

    private fun snapshotLocked(): NativeAudioState {
        val exoPlayer = player
            ?: return NativeAudioState(
                status = "idle",
                currentTime = 0.0,
                duration = 0.0,
                isPlaying = false,
                buffering = false,
                rate = 1.0,
                volume = masterVolume.toDouble(),
                gainDb = 0.0,
                trackId = null,
                nextTrackId = null,
                crossfade = crossfadeMs / 1000.0,
                outputDeviceId = outputDeviceId,
                error = null,
            )

        val rawDurationMs = exoPlayer.duration
        val durationMs = if (rawDurationMs > 0) rawDurationMs else 0L
        val currentMs = max(0L, exoPlayer.currentPosition)
        val buffering = exoPlayer.playbackState == Player.STATE_BUFFERING

        val seekState = activeSeekStateLocked()
        if (seekState?.shouldResume == true && exoPlayer.isPlaying) pendingSeekState = null

        val hasTerminalState = lastError != null || exoPlayer.playbackState == Player.STATE_ENDED
        if (hasTerminalState) pendingSeekState = null
        val effectiveIsPlaying = if (hasTerminalState) false else (seekState?.shouldResume ?: exoPlayer.isPlaying)
        val effectiveBuffering = if (hasTerminalState || seekState?.shouldResume == false) false else buffering

        val status = when {
            lastError != null -> "error"
            exoPlayer.playbackState == Player.STATE_ENDED -> "ended"
            seekState?.shouldResume == true -> "playing"
            effectiveBuffering -> "loading"
            effectiveIsPlaying -> "playing"
            else -> "idle"
        }

        val info = itemInfoOf(exoPlayer.currentMediaItem)
        // With crossfade off the next item lives in this player's own playlist,
        // so `stagedNext` is not the only place it can be read from — but it is
        // the only one that survives the item that queued it having ended.
        val nextId = stagedNext?.id
            ?: if (crossfadeMs <= 0L && exoPlayer.hasNextMediaItem()) {
                itemInfoOf(exoPlayer.getMediaItemAt(exoPlayer.currentMediaItemIndex + 1))?.id
            } else {
                null
            }

        return NativeAudioState(
            status = status,
            currentTime = currentMs / 1000.0,
            duration = durationMs / 1000.0,
            isPlaying = effectiveIsPlaying,
            buffering = effectiveBuffering,
            rate = exoPlayer.playbackParameters.speed.toDouble(),
            volume = masterVolume.toDouble(),
            gainDb = decks[activeIdx]?.gainDb ?: 0.0,
            trackId = info?.id,
            nextTrackId = nextId,
            crossfade = crossfadeMs / 1000.0,
            outputDeviceId = outputDeviceId,
            error = lastError,
        )
    }

    private fun activeSeekStateLocked(): PendingSeekState? {
        val seekState = pendingSeekState ?: return null
        val now = System.currentTimeMillis()
        if (now - seekState.startedAtMs > SEEK_STATE_STALE_MS) {
            pendingSeekState = null
            return null
        }
        return seekState
    }
}

@TauriPlugin
class NativeAudioPlugin(private val activity: Activity) : Plugin(activity) {

    init {
        activeInstance = this
    }

    @Command
    fun initialize(invoke: Invoke) {
        requestNotificationPermission()
        runCatching {
            NativeAudioRuntime.initialize(activity.applicationContext)
        }.onSuccess {
            invoke.resolve(toJsObject(NativeAudioRuntime.getState(activity.applicationContext)))
        }.onFailure {
            invoke.reject(it.message ?: "initialize failed")
        }
    }

    @Command
    fun register_listener(invoke: Invoke) {
        invoke.resolve()
    }

    @Command
    fun remove_listener(invoke: Invoke) {
        invoke.resolve()
    }

    @Command
    fun setSource(invoke: Invoke) {
        val args = invoke.parseArgs(SetSourceArgs::class.java)
        val src = args.src?.trim().orEmpty()
        if (src.isEmpty()) {
            invoke.reject("src is required")
            return
        }

        runCatching {
            NativeAudioRuntime.setSource(
                activity.applicationContext,
                src,
                args.id,
                args.title,
                args.artist,
                args.artworkUrl,
                args.gainDb,
            )
        }.onSuccess {
            invoke.resolve(toJsObject(NativeAudioRuntime.getState(activity.applicationContext)))
        }.onFailure {
            invoke.reject(it.message ?: "setSource failed")
        }
    }

    /// `src` may be omitted here — that is how the queue is cleared.
    @Command
    fun setNextSource(invoke: Invoke) {
        val args = invoke.parseArgs(SetSourceArgs::class.java)
        runCatching {
            NativeAudioRuntime.setNextSource(
                activity.applicationContext,
                args.src?.trim()?.ifEmpty { null },
                args.id,
                args.title,
                args.artist,
                args.artworkUrl,
                args.gainDb,
            )
        }.onSuccess {
            invoke.resolve(toJsObject(NativeAudioRuntime.getState(activity.applicationContext)))
        }.onFailure {
            invoke.reject(it.message ?: "setNextSource failed")
        }
    }

    @Command
    fun skipToNext(invoke: Invoke) {
        runCatching {
            NativeAudioRuntime.skipToNext(activity.applicationContext)
        }.onSuccess { moved ->
            val payload = toJsObject(NativeAudioRuntime.getState(activity.applicationContext))
            payload.put("skipped", moved)
            invoke.resolve(payload)
        }.onFailure {
            invoke.reject(it.message ?: "skipToNext failed")
        }
    }

    @Command
    fun play(invoke: Invoke) {
        runCatching {
            NativeAudioRuntime.play(activity.applicationContext)
        }.onSuccess {
            invoke.resolve(toJsObject(NativeAudioRuntime.getState(activity.applicationContext)))
        }.onFailure {
            invoke.reject(it.message ?: "play failed")
        }
    }

    @Command
    fun pause(invoke: Invoke) {
        runCatching {
            NativeAudioRuntime.pause(activity.applicationContext)
        }.onSuccess {
            invoke.resolve(toJsObject(NativeAudioRuntime.getState(activity.applicationContext)))
        }.onFailure {
            invoke.reject(it.message ?: "pause failed")
        }
    }

    @Command
    fun seekTo(invoke: Invoke) {
        val args = invoke.parseArgs(SeekToArgs::class.java)
        val position = args.position
        if (position == null || !position.isFinite()) {
            invoke.reject("position is required")
            return
        }

        runCatching {
            NativeAudioRuntime.seekTo(activity.applicationContext, position)
        }.onSuccess {
            invoke.resolve(toJsObject(NativeAudioRuntime.getState(activity.applicationContext)))
        }.onFailure {
            invoke.reject(it.message ?: "seekTo failed")
        }
    }

    @Command
    fun setRate(invoke: Invoke) {
        val args = invoke.parseArgs(SetRateArgs::class.java)
        val rate = args.rate
        if (rate == null || !rate.isFinite() || rate <= 0) {
            invoke.reject("rate must be > 0")
            return
        }

        runCatching {
            NativeAudioRuntime.setRate(activity.applicationContext, rate)
        }.onSuccess {
            invoke.resolve(toJsObject(NativeAudioRuntime.getState(activity.applicationContext)))
        }.onFailure {
            invoke.reject(it.message ?: "setRate failed")
        }
    }

    @Command
    fun setVolume(invoke: Invoke) {
        val args = invoke.parseArgs(SetVolumeArgs::class.java)
        val volume = args.volume
        if (volume == null || !volume.isFinite()) {
            invoke.reject("volume is required")
            return
        }

        runCatching {
            NativeAudioRuntime.setVolume(activity.applicationContext, volume)
        }.onSuccess {
            invoke.resolve(toJsObject(NativeAudioRuntime.getState(activity.applicationContext)))
        }.onFailure {
            invoke.reject(it.message ?: "setVolume failed")
        }
    }

    @Command
    fun setSourceGain(invoke: Invoke) {
        val args = invoke.parseArgs(SetSourceGainArgs::class.java)
        runCatching {
            NativeAudioRuntime.setSourceGain(activity.applicationContext, args.gainDb)
        }.onSuccess {
            invoke.resolve(toJsObject(NativeAudioRuntime.getState(activity.applicationContext)))
        }.onFailure {
            invoke.reject(it.message ?: "setSourceGain failed")
        }
    }

    @Command
    fun setCrossfade(invoke: Invoke) {
        val args = invoke.parseArgs(SetCrossfadeArgs::class.java)
        val seconds = args.seconds
        if (seconds == null || !seconds.isFinite() || seconds < 0) {
            invoke.reject("seconds must be >= 0")
            return
        }

        runCatching {
            NativeAudioRuntime.setCrossfade(activity.applicationContext, seconds)
        }.onSuccess {
            invoke.resolve(toJsObject(NativeAudioRuntime.getState(activity.applicationContext)))
        }.onFailure {
            invoke.reject(it.message ?: "setCrossfade failed")
        }
    }

    @Command
    fun listOutputDevices(invoke: Invoke) {
        runCatching {
            NativeAudioRuntime.listOutputDevices(activity.applicationContext)
        }.onSuccess { devices ->
            val array = JSArray()
            devices.forEach { device ->
                val item = JSObject()
                item.put("id", device.id)
                item.put("type", device.type)
                item.put("name", device.name)
                array.put(item)
            }
            val payload = JSObject()
            payload.put("devices", array)
            invoke.resolve(payload)
        }.onFailure {
            invoke.reject(it.message ?: "listOutputDevices failed")
        }
    }

    @Command
    fun setOutputDevice(invoke: Invoke) {
        val args = invoke.parseArgs(SetOutputDeviceArgs::class.java)
        runCatching {
            NativeAudioRuntime.setOutputDevice(activity.applicationContext, args.id)
        }.onSuccess {
            invoke.resolve(toJsObject(NativeAudioRuntime.getState(activity.applicationContext)))
        }.onFailure {
            invoke.reject(it.message ?: "setOutputDevice failed")
        }
    }

    @Command
    fun getState(invoke: Invoke) {
        runCatching {
            NativeAudioRuntime.getState(activity.applicationContext)
        }.onSuccess {
            invoke.resolve(toJsObject(it))
        }.onFailure {
            invoke.reject(it.message ?: "getState failed")
        }
    }

    @Command
    fun getProgressCheckpoint(invoke: Invoke) {
        runCatching {
            NativeAudioRuntime.getProgressCheckpoint(activity.applicationContext)
        }.onSuccess {
            invoke.resolve(it?.let { checkpoint -> toJsObject(checkpoint) })
        }.onFailure {
            invoke.reject(it.message ?: "getProgressCheckpoint failed")
        }
    }

    @Command
    fun clearProgressCheckpoint(invoke: Invoke) {
        runCatching {
            NativeAudioRuntime.clearProgressCheckpoint(activity.applicationContext)
        }.onSuccess {
            invoke.resolve()
        }.onFailure {
            invoke.reject(it.message ?: "clearProgressCheckpoint failed")
        }
    }

    @Command
    fun dispose(invoke: Invoke) {
        runCatching {
            NativeAudioRuntime.dispose(activity.applicationContext)
        }.onSuccess {
            invoke.resolve()
        }.onFailure {
            invoke.reject(it.message ?: "dispose failed")
        }
    }

    override fun onDestroy() {
        if (activeInstance === this) activeInstance = null
        super.onDestroy()
    }

    private fun requestNotificationPermission() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
        if (ContextCompat.checkSelfPermission(activity, Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED) return
        ActivityCompat.requestPermissions(
            activity,
            arrayOf(Manifest.permission.POST_NOTIFICATIONS),
            NOTIFICATION_PERMISSION_REQUEST_CODE,
        )
    }

    private fun emitState(state: NativeAudioState) {
        val payload = toJsObject(state)
        activity.runOnUiThread {
            trigger(EVENT_STATE, payload)
        }
    }

    private fun toJsObject(state: NativeAudioState): JSObject {
        val payload = JSObject()
        payload.put("status", state.status)
        payload.put("currentTime", state.currentTime)
        payload.put("duration", state.duration)
        payload.put("isPlaying", state.isPlaying)
        payload.put("buffering", state.buffering)
        payload.put("rate", state.rate)
        payload.put("volume", state.volume)
        payload.put("gainDb", state.gainDb)
        payload.put("crossfade", state.crossfade)
        state.trackId?.let { payload.put("trackId", it) }
        state.nextTrackId?.let { payload.put("nextTrackId", it) }
        state.outputDeviceId?.let { payload.put("outputDeviceId", it) }
        if (!state.error.isNullOrBlank()) payload.put("error", state.error)
        return payload
    }

    private fun toJsObject(checkpoint: NativeAudioProgressCheckpoint): JSObject {
        val payload = JSObject()
        payload.put("id", checkpoint.id)
        payload.put("currentTime", checkpoint.currentTime)
        payload.put("updatedAtMs", checkpoint.updatedAtMs)
        if (!checkpoint.status.isNullOrBlank()) payload.put("status", checkpoint.status)
        return payload
    }

    companion object {
        @Volatile
        private var activeInstance: NativeAudioPlugin? = null

        internal fun emitToActive(state: NativeAudioState) {
            activeInstance?.emitState(state)
        }
    }
}
