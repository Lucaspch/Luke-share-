package dev.lukeshare.stream

import android.Manifest
import android.annotation.SuppressLint
import android.app.Activity
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.util.Log
import android.webkit.JavascriptInterface
import android.webkit.WebView
import androidx.core.content.ContextCompat

/**
 * Activity principal do Luke-Share no Meta Quest 3S (Horizon OS / Android ARM64).
 * Injeta a ponte JavaScript `window.LukeQuestBridge` no WebView para disparar a captura
 * MediaProjection + corte de 1 único olho em primeiro plano mesmo ao retornar para jogos VR.
 */
class MainActivity : TauriActivity() {

    companion object {
        private const val TAG = "LukeQuestMainActivity"
    }

    private var pendingEyeMode = QuestSingleEyeRenderer.MODE_AUTO_SINGLE_EYE
    private var pendingWidth = 1920
    private var pendingHeight = 1080
    private var pendingFps = 60
    private var pendingBitrateBps = 10_000_000
    private var pendingCaptureAudio = true

    @SuppressLint("SetJavaScriptEnabled")
    override fun onWebViewCreate(webView: WebView) {
        super.onWebViewCreate(webView)
        webView.settings.mediaPlaybackRequiresUserGesture = false
        webView.addJavascriptInterface(QuestJsBridge(), "LukeQuestBridge")
        Log.i(TAG, "Ponte LukeQuestBridge registrada no WebView")
    }

    private fun startOrUpdateCapture(
        eyeMode: Int,
        width: Int,
        height: Int,
        fps: Int,
        bitrateBps: Int,
        captureAudio: Boolean
    ) {
        pendingEyeMode = eyeMode
        pendingWidth = width
        pendingHeight = height
        pendingFps = fps
        pendingBitrateBps = bitrateBps
        pendingCaptureAudio = captureAudio

        runOnUiThread {
            if (QuestCaptureService.isRunning) {
                val updateIntent = Intent(this, QuestCaptureService::class.java).apply {
                    action = QuestCaptureService.ACTION_UPDATE_EYE_MODE
                    putExtra("eyeMode", eyeMode)
                }
                startService(updateIntent)
                return@runOnUiThread
            }

            if (captureAudio &&
                ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED
            ) {
                requestPermissions(arrayOf(Manifest.permission.RECORD_AUDIO)) { _ ->
                    promptMediaProjection()
                }
            } else {
                promptMediaProjection()
            }
        }
    }

    private fun promptMediaProjection() {
        val mpm = getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
        val permIntent = mpm.createScreenCaptureIntent()
        launchActivityForResult(permIntent) { result ->
            if (result != null && result.resultCode == Activity.RESULT_OK && result.data != null) {
                val svcIntent = Intent(this, QuestCaptureService::class.java).apply {
                    action = QuestCaptureService.ACTION_START
                    putExtra("resultCode", result.resultCode)
                    putExtra("resultData", result.data)
                    putExtra("eyeMode", pendingEyeMode)
                    putExtra("width", pendingWidth)
                    putExtra("height", pendingHeight)
                    putExtra("fps", pendingFps)
                    putExtra("bitrateBps", pendingBitrateBps)
                    putExtra("captureAudio", pendingCaptureAudio)
                }
                ContextCompat.startForegroundService(this, svcIntent)
            } else {
                Log.w(TAG, "Usuário cancelou a permissão de captura de tela VR")
            }
        }
    }

    inner class QuestJsBridge {
        @JavascriptInterface
        fun isQuestDevice(): Boolean {
            return Build.MANUFACTURER.contains("oculus", ignoreCase = true) ||
                Build.BRAND.contains("meta", ignoreCase = true) ||
                Build.MODEL.contains("quest", ignoreCase = true) ||
                true
        }

        @JavascriptInterface
        fun startScreenCapture(
            eyeMode: Int,
            width: Int,
            height: Int,
            fps: Int,
            bitrateBps: Int,
            captureAudio: Boolean
        ) {
            startOrUpdateCapture(eyeMode, width, height, fps, bitrateBps, captureAudio)
        }

        @JavascriptInterface
        fun updateEyeMode(eyeMode: Int) {
            pendingEyeMode = eyeMode
            if (QuestCaptureService.isRunning) {
                val updateIntent = Intent(this@MainActivity, QuestCaptureService::class.java).apply {
                    action = QuestCaptureService.ACTION_UPDATE_EYE_MODE
                    putExtra("eyeMode", eyeMode)
                }
                startService(updateIntent)
            }
        }

        @JavascriptInterface
        fun stopScreenCapture() {
            val stopIntent = Intent(this@MainActivity, QuestCaptureService::class.java).apply {
                action = QuestCaptureService.ACTION_STOP
            }
            startService(stopIntent)
        }
    }
}
