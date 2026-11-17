package org.inkbridge.android

import android.media.MediaCodec
import android.media.MediaCodecList
import android.media.MediaFormat
import android.util.Log
import android.view.Surface
import org.inkbridge.core.ClientScreen
import org.inkbridge.core.EncodedVideoFrame
import org.inkbridge.core.Session

/**
 * Decodes the computer's H.264 stream onto a [Surface] with the platform's
 * hardware decoder, tuned for latency:
 * - `KEY_LOW_LATENCY` (Android 11+, our minimum): no decoder-side buffering;
 * - every decoded frame is rendered the moment it's available (no
 *   presentation-time scheduling, no jitter buffer: this is a LAN);
 * - the decoder is (re)created on a keyframe whenever the picture size
 *   changes (rotation), and recovers from errors by requesting a keyframe.
 */
class VideoDecoder(private val session: Session, private val surface: Surface) {

    @Volatile private var running = false
    private var thread: Thread? = null
    private var codec: MediaCodec? = null
    private var width = 0
    private var height = 0
    private val info = MediaCodec.BufferInfo()

    fun start() {
        if (running) return
        running = true
        thread = Thread(::loop, "inkbridge-decoder").apply { start() }
    }

    fun stop() {
        running = false
        thread?.join(500)
        thread = null
        release()
    }

    private fun loop() {
        session.requestKeyframe() // start from a clean picture
        while (running) {
            val frame = session.nextVideoFrame(100u) ?: continue
            try {
                decode(frame)
            } catch (e: Exception) {
                Log.w(TAG, "decoder error, resynchronising", e)
                release()
                session.requestKeyframe()
            }
        }
    }

    private fun decode(frame: EncodedVideoFrame) {
        val w = frame.width.toInt()
        val h = frame.height.toInt()
        if (codec == null || w != width || h != height) {
            if (!frame.keyframe) return // wait for a keyframe to (re)start cleanly
            release()
            codec = create(w, h)
            width = w; height = h
            // SPS/PPS go in first as codec configuration.
            val nals = splitAnnexB(frame.data)
            val config = nals.filter { it.type == NAL_SPS || it.type == NAL_PPS }
            if (config.isNotEmpty()) queue(concat(frame.data, config), 0L, MediaCodec.BUFFER_FLAG_CODEC_CONFIG)
        }
        val flags = if (frame.keyframe) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0
        queue(frame.data, frame.captureTUs.toLong(), flags)
        drain()
    }

    private fun create(w: Int, h: Int): MediaCodec {
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, w, h).apply {
            setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            setInteger(MediaFormat.KEY_PRIORITY, 0) // real-time
            // Some hardware decoders assume a 1920x1080 landscape limit unless
            // told the picture size, and refuse to start for taller pictures.
            setInteger(MediaFormat.KEY_MAX_WIDTH, w)
            setInteger(MediaFormat.KEY_MAX_HEIGHT, h)
        }
        val decoder = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        try {
            decoder.configure(format, surface, null, 0)
            decoder.start()
        } catch (e: Exception) {
            decoder.release() // a failed decoder still holds a hardware instance
            throw e
        }
        return decoder
    }

    private fun queue(data: ByteArray, ptsUs: Long, flags: Int) {
        val c = codec ?: return
        val index = c.dequeueInputBuffer(20_000)
        if (index < 0) return // decoder saturated: drop (the next keyframe heals)
        val buf = c.getInputBuffer(index) ?: return
        buf.clear()
        buf.put(data)
        c.queueInputBuffer(index, 0, data.size, ptsUs, flags)
    }

    /** Renders every decoded frame immediately. */
    private fun drain() {
        val c = codec ?: return
        while (true) {
            val index = c.dequeueOutputBuffer(info, 0)
            if (index < 0) return // try-again, format or buffers changed
            c.releaseOutputBuffer(index, true)
            // presentationTimeUs carries the computer's capture time.
            session.framePresented(info.presentationTimeUs.toULong())
        }
    }

    private fun release() {
        codec?.let { runCatching { it.stop() }; runCatching { it.release() } }
        codec = null
        width = 0; height = 0
    }

    private data class Nal(val start: Int, val end: Int, val type: Int)

    /** Splits an Annex-B access unit into NAL units (offsets include start codes). */
    private fun splitAnnexB(d: ByteArray): List<Nal> {
        val starts = ArrayList<Int>()
        var i = 0
        while (i + 3 < d.size) {
            if (d[i].toInt() == 0 && d[i + 1].toInt() == 0 &&
                (d[i + 2].toInt() == 1 || (d[i + 2].toInt() == 0 && d[i + 3].toInt() == 1))
            ) {
                starts += i
                i += if (d[i + 2].toInt() == 1) 3 else 4
            } else i++
        }
        return starts.mapIndexed { n, s ->
            val payload = if (d[s + 2].toInt() == 1) s + 3 else s + 4
            Nal(s, if (n + 1 < starts.size) starts[n + 1] else d.size, d[payload].toInt() and 0x1F)
        }
    }

    private fun concat(d: ByteArray, nals: List<Nal>): ByteArray =
        nals.fold(ByteArray(0)) { acc, n -> acc + d.copyOfRange(n.start, n.end) }

    companion object {
        /**
         * The screen size plus the largest picture this device's H.264
         * decoder accepts, which the computer must not exceed. Many tablets
         * stop at 1920x1088 (either way up) even with a larger screen.
         */
        fun screen(width: Int, height: Int): ClientScreen {
            val limit = runCatching {
                val codecs = MediaCodecList(MediaCodecList.REGULAR_CODECS)
                val probe = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, 1280, 720)
                val name = codecs.findDecoderForFormat(probe)
                val video = codecs.codecInfos.first { it.name == name }
                    .getCapabilitiesForType(MediaFormat.MIMETYPE_VIDEO_AVC).videoCapabilities
                val long = video.supportedWidths.upper
                long to video.getSupportedHeightsFor(long).upper
            }.getOrNull()
            val long = maxOf(limit?.first ?: 0, limit?.second ?: 0)
            val short = minOf(limit?.first ?: 0, limit?.second ?: 0)
            Log.i(TAG, "decoder limit ${long}x$short, screen ${width}x$height")
            return ClientScreen(width.toUInt(), height.toUInt(), long.toUInt(), short.toUInt())
        }

        private const val TAG = "InkbridgeVideo"
        private const val NAL_SPS = 7
        private const val NAL_PPS = 8
    }
}
