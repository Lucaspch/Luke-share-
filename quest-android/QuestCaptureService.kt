package dev.lukeshare.stream

import android.Manifest
import android.annotation.SuppressLint
import android.app.Activity
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.graphics.Point
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioPlaybackCaptureConfiguration
import android.media.AudioRecord
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import android.os.Process
import android.util.Log
import android.view.WindowManager
import androidx.core.content.ContextCompat
import kotlin.math.abs

/**
 * Serviço em primeiro plano do Meta Quest 3S responsável por:
 * 1. Capturar a visão VR via MediaProjection + VirtualDisplay sem tela dupla (via QuestSingleEyeRenderer).
 * 2. Codificar vídeo H.264 em baixa latência no hardware do Snapdragon XR2 Gen 2 (MediaCodec).
 * 3. Capturar o áudio interno dos jogos VR via AudioPlaybackCapture (excluindo o UID do próprio app).
 * 4. Despachar quadros e áudio PCM direto para o pipeline Rust P2P via JNI.
 */
class QuestCaptureService : Service() {

    companion object {
        private const val TAG = "QuestCaptureService"
        private const val CHANNEL_ID = "luke_share_vr_capture"
        private const val NOTIF_ID = 1001

        const val ACTION_START = "dev.lukeshare.stream.action.START_CAPTURE"
        const val ACTION_UPDATE_EYE_MODE = "dev.lukeshare.stream.action.UPDATE_EYE_MODE"
        const val ACTION_STOP = "dev.lukeshare.stream.action.STOP_CAPTURE"

        @Volatile
        var isRunning: Boolean = false
            private set

        init {
            try {
                System.loadLibrary("luke_share_lib")
            } catch (e: UnsatisfiedLinkError) {
                Log.w(TAG, "libluke_share_lib ainda não carregada diretamente pelo serviço: ${e.message}")
            }
        }

        @JvmStatic
        external fun nativePushVideoFrame(isKey: Boolean, ptsUs: Long, data: ByteArray)

        @JvmStatic
        external fun nativePushAudioPcm(samples: FloatArray, count: Int)

        @JvmStatic
        external fun nativeConsumeKeyframeRequest(): Boolean

        @JvmStatic
        external fun nativeGetTargetBitrate(): Int

        @JvmStatic
        external fun nativeGetActiveEyeMode(): Int
    }

    @Volatile
    private var capturing = false

    private var projection: MediaProjection? = null
    private var virtualDisplay: VirtualDisplay? = null
    private var codec: MediaCodec? = null
    private var renderer: QuestSingleEyeRenderer? = null
    private var audioRecord: AudioRecord? = null

    private var videoThread: Thread? = null
    private var audioThread: Thread? = null

    private var wakeLock: PowerManager.WakeLock? = null
    private var wifiLock: WifiManager.WifiLock? = null

    private val projectionCallback = object : MediaProjection.Callback() {
        override fun onStop() {
            Log.i(TAG, "MediaProjection encerrado pelo sistema Horizon OS")
            stopCaptureInternal()
            stopSelf()
        }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_STOP -> {
                stopCaptureInternal()
                stopSelf()
                return START_NOT_STICKY
            }
            ACTION_UPDATE_EYE_MODE -> {
                val mode = intent.getIntExtra("eyeMode", QuestSingleEyeRenderer.MODE_AUTO_SINGLE_EYE)
                renderer?.setEyeMode(mode)
                return START_NOT_STICKY
            }
            ACTION_START -> {
                val resultCode = intent.getIntExtra("resultCode", Activity.RESULT_CANCELED)
                @Suppress("DEPRECATION")
                val resultData: Intent? = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                    intent.getParcelableExtra("resultData", Intent::class.java)
                } else {
                    intent.getParcelableExtra("resultData")
                }

                if (resultCode != Activity.RESULT_OK || resultData == null) {
                    Log.w(TAG, "Permissão MediaProjection negada ou inválida")
                    stopSelf()
                    return START_NOT_STICKY
                }

                val eyeMode = intent.getIntExtra("eyeMode", QuestSingleEyeRenderer.MODE_AUTO_SINGLE_EYE)
                val reqWidth = alignEven(intent.getIntExtra("width", 1920).coerceIn(480, 1920))
                val reqHeight = alignEven(intent.getIntExtra("height", 1080).coerceIn(360, 1080))
                val fps = intent.getIntExtra("fps", 60).coerceIn(24, 90)
                val bitrateBps = intent.getIntExtra("bitrateBps", 10_000_000).coerceIn(1_500_000, 30_000_000)
                val captureAudio = intent.getBooleanExtra("captureAudio", true)

                promoteToForeground()
                stopCaptureInternal()
                startCaptureInternal(resultCode, resultData, eyeMode, reqWidth, reqHeight, fps, bitrateBps, captureAudio)
            }
        }
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        stopCaptureInternal()
        super.onDestroy()
    }

    private fun alignEven(v: Int): Int = if (v % 2 == 0) v else v - 1

    private fun promoteToForeground() {
        val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val ch = NotificationChannel(
            CHANNEL_ID,
            "Luke-Share VR Stream",
            NotificationManager.IMPORTANCE_LOW
        ).apply {
            description = "Mantém a transmissão P2P do Meta Quest ativa em segundo plano"
            setSound(null, null)
        }
        nm.createNotificationChannel(ch)

        val notif = Notification.Builder(this, CHANNEL_ID)
            .setContentTitle("Luke-Share VR Ao Vivo")
            .setContentText("Transmitindo visão de tela única P2P")
            .setSmallIcon(android.R.drawable.ic_menu_camera)
            .setOngoing(true)
            .build()

        startForeground(NOTIF_ID, notif, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION)
    }

    private fun acquireLocks() {
        val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
        wakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "LukeShare::VrCaptureWakeLock").apply {
            setReferenceCounted(false)
            acquire()
        }

        val wm = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
        val lockMode = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            WifiManager.WIFI_MODE_FULL_LOW_LATENCY
        } else {
            @Suppress("DEPRECATION")
            WifiManager.WIFI_MODE_FULL_HIGH_PERF
        }
        wifiLock = wm.createWifiLock(lockMode, "LukeShare::VrCaptureWifiLock").apply {
            setReferenceCounted(false)
            acquire()
        }
    }

    private fun releaseLocks() {
        try {
            if (wakeLock?.isHeld == true) wakeLock?.release()
        } catch (_: Throwable) {
        }
        wakeLock = null

        try {
            if (wifiLock?.isHeld == true) wifiLock?.release()
        } catch (_: Throwable) {
        }
        wifiLock = null
    }

    private fun startCaptureInternal(
        resultCode: Int,
        resultData: Intent,
        eyeMode: Int,
        outWidth: Int,
        outHeight: Int,
        fps: Int,
        bitrateBps: Int,
        captureAudio: Boolean
    ) {
        try {
            acquireLocks()
            val mpm = getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
            val proj = mpm.getMediaProjection(resultCode, resultData)
            if (proj == null) {
                Log.e(TAG, "getMediaProjection retornou nulo")
                stopCaptureInternal()
                return
            }
            proj.registerCallback(projectionCallback, Handler(Looper.getMainLooper()))
            projection = proj

            // Descobre resolução física do compositor para auxiliar a detecção automática de SBS
            val rawSize = Point(outWidth, outHeight)
            val wm = getSystemService(Context.WINDOW_SERVICE) as WindowManager
            @Suppress("DEPRECATION")
            wm.defaultDisplay?.getRealSize(rawSize)

            // Prepara encoder H.264 por hardware em Surface
            val fmt = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, outWidth, outHeight).apply {
                setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
                setInteger(MediaFormat.KEY_BIT_RATE, bitrateBps)
                setInteger(MediaFormat.KEY_FRAME_RATE, fps)
                setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 2)
                try {
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                        setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
                    }
                    setInteger(MediaFormat.KEY_PRIORITY, 0)
                } catch (_: Throwable) {
                }
            }

            val mc = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
            mc.configure(fmt, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            val encInputSurface = mc.createInputSurface()
            mc.start()
            codec = mc

            // Prepara renderizador OpenGL de Olho Único (elimina tela dupla quando SBS ativo)
            val eyeRenderer = QuestSingleEyeRenderer(
                encoderInputSurface = encInputSurface,
                outWidth = outWidth,
                outHeight = outHeight,
                rawDisplayWidth = rawSize.x,
                rawDisplayHeight = rawSize.y,
                initialEyeMode = eyeMode
            )
            val vdWidth = if (rawSize.x >= rawSize.y * 1.75f) alignEven(rawSize.x.coerceAtMost(2560)) else outWidth
            val vdHeight = if (rawSize.x >= rawSize.y * 1.75f) alignEven(rawSize.y.coerceAtMost(1080)) else outHeight
            val captureSurface = eyeRenderer.start(vdWidth, vdHeight)
            renderer = eyeRenderer

            val density = resources.displayMetrics.densityDpi
            virtualDisplay = proj.createVirtualDisplay(
                "LukeShareVR",
                vdWidth,
                vdHeight,
                density,
                DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
                captureSurface,
                null,
                null
            )

            capturing = true
            isRunning = true

            startVideoDrainLoop(mc, bitrateBps)
            if (captureAudio) {
                startAudioCaptureLoop(proj)
            }
            Log.i(TAG, "Captura VR iniciada (${outWidth}x${outHeight} @ ${fps}fps, modoOlho=$eyeMode)")
        } catch (e: Throwable) {
            Log.e(TAG, "Falha ao iniciar captura Quest 3S", e)
            stopCaptureInternal()
            stopSelf()
        }
    }

    private fun startVideoDrainLoop(mc: MediaCodec, initialBitrate: Int) {
        videoThread = Thread({
            val info = MediaCodec.BufferInfo()
            var lastBitrate = initialBitrate
            var lastEyeMode = -1

            while (capturing) {
                try {
                    if (nativeConsumeKeyframeRequest()) {
                        val b = Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0) }
                        mc.setParameters(b)
                    }

                    val targetBr = nativeGetTargetBitrate()
                    if (targetBr > 0 && abs(targetBr - lastBitrate) > lastBitrate / 20) {
                        lastBitrate = targetBr
                        val b = Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_VIDEO_BITRATE, targetBr) }
                        mc.setParameters(b)
                    }

                    val mode = nativeGetActiveEyeMode()
                    if (mode in 1..4 && mode != lastEyeMode) {
                        lastEyeMode = mode
                        renderer?.setEyeMode(mode)
                    }

                    val outIdx = mc.dequeueOutputBuffer(info, 10_000L)
                    if (outIdx >= 0) {
                        if (info.size > 0) {
                            val outBuf = mc.getOutputBuffer(outIdx)
                            if (outBuf != null) {
                                outBuf.position(info.offset)
                                outBuf.limit(info.offset + info.size)
                                val bytes = ByteArray(info.size)
                                outBuf.get(bytes)

                                val isKey = (info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME) != 0
                                nativePushVideoFrame(isKey, info.presentationTimeUs, bytes)
                            }
                        }
                        mc.releaseOutputBuffer(outIdx, false)
                    }
                } catch (e: Throwable) {
                    if (capturing) {
                        Log.w(TAG, "Exceção no dreno MediaCodec", e)
                    }
                }
            }
        }, "QuestVideoDrain").apply { start() }
    }

    @SuppressLint("MissingPermission")
    private fun startAudioCaptureLoop(proj: MediaProjection) {
        if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            Log.w(TAG, "RECORD_AUDIO não concedido — transmissão VR seguirá somente com vídeo")
            return
        }

        try {
            val apc = AudioPlaybackCaptureConfiguration.Builder(proj)
                .addMatchingUsage(AudioAttributes.USAGE_MEDIA)
                .addMatchingUsage(AudioAttributes.USAGE_GAME)
                .addMatchingUsage(AudioAttributes.USAGE_UNKNOWN)
                .excludeUid(Process.myUid())
                .build()

            val fmt = AudioFormat.Builder()
                .setEncoding(AudioFormat.ENCODING_PCM_FLOAT)
                .setSampleRate(48_000)
                .setChannelMask(AudioFormat.CHANNEL_IN_STEREO)
                .build()

            val minBuf = AudioRecord.getMinBufferSize(48_000, AudioFormat.CHANNEL_IN_STEREO, AudioFormat.ENCODING_PCM_FLOAT)
            val chunkFloats = 960 * 2 // 20 ms estéreo
            val rec = AudioRecord.Builder()
                .setAudioPlaybackCaptureConfig(apc)
                .setAudioFormat(fmt)
                .setBufferSizeInBytes(maxOf(minBuf * 2, chunkFloats * 4 * 4))
                .build()

            rec.startRecording()
            audioRecord = rec

            audioThread = Thread({
                val pcm = FloatArray(chunkFloats)
                while (capturing) {
                    val read = try {
                        rec.read(pcm, 0, pcm.size, AudioRecord.READ_BLOCKING)
                    } catch (_: Throwable) {
                        -1
                    }
                    if (read > 0) {
                        nativePushAudioPcm(pcm, read)
                    } else if (read < 0) {
                        break
                    }
                }
            }, "QuestAudioCapture").apply { start() }
        } catch (e: Throwable) {
            Log.w(TAG, "AudioPlaybackCapture indisponível nesta build do sistema", e)
        }
    }

    private fun stopCaptureInternal() {
        capturing = false
        isRunning = false

        try {
            audioRecord?.stop()
        } catch (_: Throwable) {
        }
        try {
            audioRecord?.release()
        } catch (_: Throwable) {
        }
        audioRecord = null

        try {
            audioThread?.join(1000)
        } catch (_: Throwable) {
        }
        audioThread = null

        try {
            renderer?.release()
        } catch (_: Throwable) {
        }
        renderer = null

        try {
            virtualDisplay?.release()
        } catch (_: Throwable) {
        }
        virtualDisplay = null

        try {
            codec?.stop()
        } catch (_: Throwable) {
        }
        try {
            codec?.release()
        } catch (_: Throwable) {
        }
        codec = null

        try {
            projection?.unregisterCallback(projectionCallback)
            projection?.stop()
        } catch (_: Throwable) {
        }
        projection = null

        try {
            videoThread?.join(1000)
        } catch (_: Throwable) {
        }
        videoThread = null

        releaseLocks()
    }
}
