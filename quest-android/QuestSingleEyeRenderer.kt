package dev.lukeshare.stream

import android.graphics.SurfaceTexture
import android.opengl.EGL14
import android.opengl.EGLConfig
import android.opengl.EGLContext
import android.opengl.EGLDisplay
import android.opengl.EGLExt
import android.opengl.EGLSurface
import android.opengl.GLES11Ext
import android.opengl.GLES20
import android.util.Log
import android.view.Surface
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.nio.FloatBuffer
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import kotlin.math.abs
import kotlin.math.sqrt

/**
 * Renderizador OpenGL ES 2.0 que garante envio de apenas 1 TELA (1 olho) para o MediaCodec H.264,
 * eliminando automaticamente tela dupla Side-by-Side (olho esquerdo + olho direito) quando presente no Quest 3S.
 */
class QuestSingleEyeRenderer(
    private val encoderInputSurface: Surface,
    private val outWidth: Int,
    private val outHeight: Int,
    private val rawDisplayWidth: Int,
    private val rawDisplayHeight: Int,
    initialEyeMode: Int = MODE_AUTO_SINGLE_EYE
) {
    companion object {
        private const val TAG = "QuestSingleEyeRenderer"
        const val MODE_AUTO_SINGLE_EYE = 1
        const val MODE_FORCE_LEFT_EYE = 2
        const val MODE_FORCE_RIGHT_EYE = 3
        const val MODE_FULL_2D = 4

        private const val EGL_RECORDABLE_ANDROID = 0x3142
        private const val PROBE_W = 64
        private const val PROBE_H = 32

        private const val VERTEX_SHADER = """
            attribute vec4 aPosition;
            attribute vec2 aTexCoord;
            uniform mat4 uTexMatrix;
            uniform vec4 uCropRect;
            varying vec2 vTexCoord;
            void main() {
                gl_Position = aPosition;
                vec2 cropped = vec2(
                    uCropRect.x + aTexCoord.x * uCropRect.z,
                    uCropRect.y + aTexCoord.y * uCropRect.w
                );
                vTexCoord = (uTexMatrix * vec4(cropped, 0.0, 1.0)).xy;
            }
        """

        private const val FRAGMENT_SHADER = """
            #extension GL_OES_EGL_image_external : require
            precision mediump float;
            uniform samplerExternalOES sTexture;
            varying vec2 vTexCoord;
            void main() {
                gl_FragColor = texture2D(sTexture, vTexCoord);
            }
        """
    }

    @Volatile
    private var eyeMode: Int = initialEyeMode

    @Volatile
    private var running: Boolean = false

    @Volatile
    private var frameAvailable: Boolean = false

    private val frameLock = Object()
    private var renderThread: Thread? = null

    @Volatile
    var inputSurface: Surface? = null
        private set

    private var surfaceTexture: SurfaceTexture? = null
    private var eglDisplay: EGLDisplay = EGL14.EGL_NO_DISPLAY
    private var eglContext: EGLContext = EGL14.EGL_NO_CONTEXT
    private var eglSurface: EGLSurface = EGL14.EGL_NO_SURFACE

    private var program = 0
    private var aPositionLoc = 0
    private var aTexCoordLoc = 0
    private var uTexMatrixLoc = 0
    private var uCropRectLoc = 0
    private var oesTexId = 0

    private var probeFbo = 0
    private var probeTex = 0
    private val probeBuf: ByteBuffer = ByteBuffer.allocateDirect(PROBE_W * PROBE_H * 4).order(ByteOrder.nativeOrder())

    private val quadVertices: FloatBuffer = ByteBuffer.allocateDirect(16 * 4)
        .order(ByteOrder.nativeOrder())
        .asFloatBuffer()
        .apply {
            // x, y, u, v (TRIANGLE_STRIP)
            put(
                floatArrayOf(
                    -1f, -1f, 0f, 0f,
                     1f, -1f, 1f, 0f,
                    -1f,  1f, 0f, 1f,
                     1f,  1f, 1f, 1f
                )
            )
            position(0)
        }

    fun setEyeMode(mode: Int) {
        eyeMode = mode.coerceIn(MODE_AUTO_SINGLE_EYE, MODE_FULL_2D)
    }

    fun start(virtualDisplayWidth: Int, virtualDisplayHeight: Int): Surface {
        val readyLatch = CountDownLatch(1)
        var initError: Throwable? = null
        running = true

        val t = Thread({
            try {
                initEgl()
                initGl()

                val st = SurfaceTexture(oesTexId)
                st.setDefaultBufferSize(virtualDisplayWidth, virtualDisplayHeight)
                st.setOnFrameAvailableListener {
                    synchronized(frameLock) {
                        frameAvailable = true
                        frameLock.notifyAll()
                    }
                }
                surfaceTexture = st
                inputSurface = Surface(st)
                readyLatch.countDown()

                renderLoop(st)
            } catch (e: Throwable) {
                initError = e
                Log.e(TAG, "Erro no loop GL de captura VR", e)
                readyLatch.countDown()
            } finally {
                cleanupGlAndEgl()
            }
        }, "QuestEyeGL")
        renderThread = t
        t.start()

        readyLatch.await(5, TimeUnit.SECONDS)
        initError?.let { throw IllegalStateException("Falha inicializando renderizador OpenGL VR", it) }
        return inputSurface ?: throw IllegalStateException("Surface de captura não foi inicializada")
    }

    fun release() {
        running = false
        synchronized(frameLock) {
            frameLock.notifyAll()
        }
        try {
            renderThread?.join(2000)
        } catch (_: InterruptedException) {
        }
        renderThread = null
    }

    private fun renderLoop(st: SurfaceTexture) {
        val texMatrix = FloatArray(16)
        var frameCounter = 0L
        var autoDetectedSbs = rawDisplayWidth > 0 &&
            rawDisplayHeight > 0 &&
            rawDisplayWidth.toFloat() >= rawDisplayHeight.toFloat() * 1.75f
        var nonSbsStreak = 0

        while (running) {
            var hasNewFrame = false
            synchronized(frameLock) {
                if (!frameAvailable && running) {
                    try {
                        frameLock.wait(250)
                    } catch (_: InterruptedException) {
                    }
                }
                if (frameAvailable) {
                    frameAvailable = false
                    hasNewFrame = true
                }
            }

            if (!running) break
            if (!hasNewFrame) continue

            st.updateTexImage()
            st.getTransformMatrix(texMatrix)
            frameCounter++

            // No Modo 1 (Auto 1 Tela), amostra a cada 30 quadros se não for display físico wide óbvio
            if (eyeMode == MODE_AUTO_SINGLE_EYE &&
                !(rawDisplayWidth > 0 && rawDisplayHeight > 0 && rawDisplayWidth.toFloat() >= rawDisplayHeight.toFloat() * 1.75f) &&
                (frameCounter == 1L || frameCounter % 30L == 0L)
            ) {
                val verdict = detectSideBySideStereo(texMatrix)
                if (verdict == true) {
                    autoDetectedSbs = true
                    nonSbsStreak = 0
                } else if (verdict == false) {
                    nonSbsStreak++
                    if (nonSbsStreak >= 5) {
                        autoDetectedSbs = false
                    }
                }
            }

            val crop = when (eyeMode) {
                MODE_FORCE_LEFT_EYE -> floatArrayOf(0.02f, 0.04f, 0.46f, 0.92f)
                MODE_FORCE_RIGHT_EYE -> floatArrayOf(0.52f, 0.04f, 0.46f, 0.92f)
                MODE_FULL_2D -> floatArrayOf(0.0f, 0.0f, 1.0f, 1.0f)
                else -> {
                    if (autoDetectedSbs) {
                        floatArrayOf(0.02f, 0.04f, 0.46f, 0.92f)
                    } else {
                        floatArrayOf(0.0f, 0.0f, 1.0f, 1.0f)
                    }
                }
            }

            GLES20.glBindFramebuffer(GLES20.GL_FRAMEBUFFER, 0)
            GLES20.glViewport(0, 0, outWidth, outHeight)
            GLES20.glClearColor(0f, 0f, 0f, 1f)
            GLES20.glClear(GLES20.GL_COLOR_BUFFER_BIT)
            drawQuad(texMatrix, crop[0], crop[1], crop[2], crop[3])

            val ptsNs = st.timestamp
            if (ptsNs > 0) {
                EGLExt.eglPresentationTimeANDROID(eglDisplay, eglSurface, ptsNs)
            }
            EGL14.eglSwapBuffers(eglDisplay, eglSurface)
        }
    }

    /**
     * Desenha o frame completo num FBO 64x32 e compara correlação horizontal entre a metade esquerda
     * e direita com disparidade de -3..+3 px. Retorna `true` se for visão estéreo 2 olhos SBS.
     */
    private fun detectSideBySideStereo(texMatrix: FloatArray): Boolean? {
        if (probeFbo == 0) return null
        GLES20.glBindFramebuffer(GLES20.GL_FRAMEBUFFER, probeFbo)
        GLES20.glViewport(0, 0, PROBE_W, PROBE_H)
        drawQuad(texMatrix, 0f, 0f, 1f, 1f)

        probeBuf.clear()
        GLES20.glReadPixels(0, 0, PROBE_W, PROBE_H, GLES20.GL_RGBA, GLES20.GL_UNSIGNED_BYTE, probeBuf)
        probeBuf.rewind()

        val luma = IntArray(PROBE_W * PROBE_H)
        for (i in luma.indices) {
            val r = probeBuf.get().toInt() and 0xFF
            val g = probeBuf.get().toInt() and 0xFF
            val b = probeBuf.get().toInt() and 0xFF
            probeBuf.get() // alpha
            luma[i] = (77 * r + 150 * g + 29 * b) shr 8
        }

        // Calcula variância na metade esquerda (evita classificar tela preta/uniforme como estéreo)
        var sum = 0.0
        var count = 0
        for (y in 4 until (PROBE_H - 4)) {
            val rowOff = y * PROBE_W
            for (x in 4 until 28) {
                sum += luma[rowOff + x]
                count++
            }
        }
        val mean = sum / count.coerceAtLeast(1)
        var sqSum = 0.0
        for (y in 4 until (PROBE_H - 4)) {
            val rowOff = y * PROBE_W
            for (x in 4 until 28) {
                val d = luma[rowOff + x] - mean
                sqSum += d * d
            }
        }
        val stdDev = sqrt(sqSum / count.coerceAtLeast(1))
        if (stdDev <= 8.0) {
            return null // Tela quase uniforme/escura — inconclusivo
        }

        var minMad = Double.MAX_VALUE
        for (dx in -3..3) {
            var diffSum = 0.0
            var cmpCount = 0
            for (y in 4 until (PROBE_H - 4)) {
                val rowOff = y * PROBE_W
                for (x in 4 until 28) {
                    val rx = x + 32 + dx
                    if (rx in 32 until PROBE_W) {
                        diffSum += abs(luma[rowOff + x] - luma[rowOff + rx])
                        cmpCount++
                    }
                }
            }
            if (cmpCount > 0) {
                val mad = diffSum / cmpCount
                if (mad < minMad) minMad = mad
            }
        }

        return minMad < 22.0
    }

    private fun drawQuad(texMatrix: FloatArray, uMin: Float, vMin: Float, uScale: Float, vScale: Float) {
        GLES20.glUseProgram(program)
        GLES20.glActiveTexture(GLES20.GL_TEXTURE0)
        GLES20.glBindTexture(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, oesTexId)

        GLES20.glUniformMatrix4fv(uTexMatrixLoc, 1, false, texMatrix, 0)
        GLES20.glUniform4f(uCropRectLoc, uMin, vMin, uScale, vScale)

        quadVertices.position(0)
        GLES20.glVertexAttribPointer(aPositionLoc, 2, GLES20.GL_FLOAT, false, 16, quadVertices)
        GLES20.glEnableVertexAttribArray(aPositionLoc)

        quadVertices.position(2)
        GLES20.glVertexAttribPointer(aTexCoordLoc, 2, GLES20.GL_FLOAT, false, 16, quadVertices)
        GLES20.glEnableVertexAttribArray(aTexCoordLoc)

        GLES20.glDrawArrays(GLES20.GL_TRIANGLE_STRIP, 0, 4)
    }

    private fun initEgl() {
        eglDisplay = EGL14.eglGetDisplay(EGL14.EGL_DEFAULT_DISPLAY)
        require(eglDisplay != EGL14.EGL_NO_DISPLAY) { "eglGetDisplay falhou" }
        val version = IntArray(2)
        require(EGL14.eglInitialize(eglDisplay, version, 0, version, 1)) { "eglInitialize falhou" }

        val attribList = intArrayOf(
            EGL14.EGL_RED_SIZE, 8,
            EGL14.EGL_GREEN_SIZE, 8,
            EGL14.EGL_BLUE_SIZE, 8,
            EGL14.EGL_ALPHA_SIZE, 8,
            EGL14.EGL_RENDERABLE_TYPE, EGL14.EGL_OPENGL_ES2_BIT,
            EGL_RECORDABLE_ANDROID, 1,
            EGL14.EGL_NONE
        )
        val configs = arrayOfNulls<EGLConfig>(1)
        val numConfigs = IntArray(1)
        require(EGL14.eglChooseConfig(eglDisplay, attribList, 0, configs, 0, 1, numConfigs, 0) && numConfigs[0] > 0) {
            "eglChooseConfig falhou"
        }

        val ctxAttribs = intArrayOf(EGL14.EGL_CONTEXT_CLIENT_VERSION, 2, EGL14.EGL_NONE)
        eglContext = EGL14.eglCreateContext(eglDisplay, configs[0], EGL14.EGL_NO_CONTEXT, ctxAttribs, 0)
        require(eglContext != EGL14.EGL_NO_CONTEXT) { "eglCreateContext falhou" }

        val surfAttribs = intArrayOf(EGL14.EGL_NONE)
        eglSurface = EGL14.eglCreateWindowSurface(eglDisplay, configs[0], encoderInputSurface, surfAttribs, 0)
        require(eglSurface != EGL14.EGL_NO_SURFACE) { "eglCreateWindowSurface falhou" }

        require(EGL14.eglMakeCurrent(eglDisplay, eglSurface, eglSurface, eglContext)) {
            "eglMakeCurrent falhou"
        }
    }

    private fun initGl() {
        val vs = compileShader(GLES20.GL_VERTEX_SHADER, VERTEX_SHADER)
        val fs = compileShader(GLES20.GL_FRAGMENT_SHADER, FRAGMENT_SHADER)
        program = GLES20.glCreateProgram()
        GLES20.glAttachShader(program, vs)
        GLES20.glAttachShader(program, fs)
        GLES20.glLinkProgram(program)
        GLES20.glDeleteShader(vs)
        GLES20.glDeleteShader(fs)

        aPositionLoc = GLES20.glGetAttribLocation(program, "aPosition")
        aTexCoordLoc = GLES20.glGetAttribLocation(program, "aTexCoord")
        uTexMatrixLoc = GLES20.glGetUniformLocation(program, "uTexMatrix")
        uCropRectLoc = GLES20.glGetUniformLocation(program, "uCropRect")

        val texIds = IntArray(2)
        GLES20.glGenTextures(2, texIds, 0)
        oesTexId = texIds[0]
        GLES20.glBindTexture(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, oesTexId)
        GLES20.glTexParameteri(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, GLES20.GL_TEXTURE_MIN_FILTER, GLES20.GL_LINEAR)
        GLES20.glTexParameteri(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, GLES20.GL_TEXTURE_MAG_FILTER, GLES20.GL_LINEAR)
        GLES20.glTexParameteri(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, GLES20.GL_TEXTURE_WRAP_S, GLES20.GL_CLAMP_TO_EDGE)
        GLES20.glTexParameteri(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, GLES20.GL_TEXTURE_WRAP_T, GLES20.GL_CLAMP_TO_EDGE)

        // Textura e FBO para detecção SBS 64x32
        probeTex = texIds[1]
        GLES20.glBindTexture(GLES20.GL_TEXTURE_2D, probeTex)
        GLES20.glTexImage2D(GLES20.GL_TEXTURE_2D, 0, GLES20.GL_RGBA, PROBE_W, PROBE_H, 0, GLES20.GL_RGBA, GLES20.GL_UNSIGNED_BYTE, null)
        GLES20.glTexParameteri(GLES20.GL_TEXTURE_2D, GLES20.GL_TEXTURE_MIN_FILTER, GLES20.GL_LINEAR)
        GLES20.glTexParameteri(GLES20.GL_TEXTURE_2D, GLES20.GL_TEXTURE_MAG_FILTER, GLES20.GL_LINEAR)

        val fbos = IntArray(1)
        GLES20.glGenFramebuffers(1, fbos, 0)
        probeFbo = fbos[0]
        GLES20.glBindFramebuffer(GLES20.GL_FRAMEBUFFER, probeFbo)
        GLES20.glFramebufferTexture2D(GLES20.GL_FRAMEBUFFER, GLES20.GL_COLOR_ATTACHMENT0, GLES20.GL_TEXTURE_2D, probeTex, 0)
        GLES20.glBindFramebuffer(GLES20.GL_FRAMEBUFFER, 0)
    }

    private fun compileShader(type: Int, src: String): Int {
        val shader = GLES20.glCreateShader(type)
        GLES20.glShaderSource(shader, src)
        GLES20.glCompileShader(shader)
        return shader
    }

    private fun cleanupGlAndEgl() {
        try {
            inputSurface?.release()
        } catch (_: Throwable) {
        }
        inputSurface = null

        try {
            surfaceTexture?.release()
        } catch (_: Throwable) {
        }
        surfaceTexture = null

        if (eglDisplay != EGL14.EGL_NO_DISPLAY) {
            if (probeFbo != 0) GLES20.glDeleteFramebuffers(1, intArrayOf(probeFbo), 0)
            if (oesTexId != 0 || probeTex != 0) GLES20.glDeleteTextures(2, intArrayOf(oesTexId, probeTex), 0)
            if (program != 0) GLES20.glDeleteProgram(program)

            EGL14.eglMakeCurrent(eglDisplay, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_CONTEXT)
            if (eglSurface != EGL14.EGL_NO_SURFACE) EGL14.eglDestroySurface(eglDisplay, eglSurface)
            if (eglContext != EGL14.EGL_NO_CONTEXT) EGL14.eglDestroyContext(eglDisplay, eglContext)
            EGL14.eglTerminate(eglDisplay)
        }
        eglSurface = EGL14.EGL_NO_SURFACE
        eglContext = EGL14.EGL_NO_CONTEXT
        eglDisplay = EGL14.EGL_NO_DISPLAY
    }
}
