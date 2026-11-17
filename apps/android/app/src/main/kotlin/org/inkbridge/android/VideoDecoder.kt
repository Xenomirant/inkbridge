package org.inkbridge.android

import android.media.MediaCodec
import android.media.MediaCodecInfo
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

    // Time each frame spends in the decoder (logged for latency work).
    private val queuedAt = HashMap<Long, Long>()
    private var decodeCount = 0
    private var decodeSum = 0.0
    private var decodeMax = 0.0

    /** Frames given to the decoder that it hasn't returned yet. */
    private var pending = 0

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
            // While the decoder still holds a frame, come back quickly to
            // show it: otherwise it would wait for the next frame to arrive,
            // which on a still screen can be much later.
            val frame = session.nextVideoFrame(if (pending > 0) 4u else 100u)
            try {
                if (frame == null) { drain(0); continue }
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
        if (queue(frame.data, frame.captureTUs.toLong(), flags)) pending++
        drain(0)
    }

    private fun create(w: Int, h: Int): MediaCodec {
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, w, h).apply {
            setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            setInteger(MediaFormat.KEY_PRIORITY, 0) // real-time
            // Some hardware decoders assume a 1920x1080 landscape limit unless
            // told the picture size, and refuse to start for taller pictures.
            setInteger(MediaFormat.KEY_MAX_WIDTH, w)
            setInteger(MediaFormat.KEY_MAX_HEIGHT, h)
            // Run the decoder at full speed instead of pacing it to the
            // frame rate, and hand frames out in decode order at once
            // (the stream has no reordering). The vendor keys are Qualcomm's;
            // other decoders ignore them.
            setInteger(MediaFormat.KEY_OPERATING_RATE, Short.MAX_VALUE.toInt())
            setInteger("vendor.qti-ext-dec-low-latency.enable", 1)
            setInteger("vendor.qti-ext-dec-picture-order.enable", 1)
        }
        // Some chips have a separate low-latency decoder that the default
        // choice skips; ask for one that supports the feature.
        val lowLatency = runCatching {
            val wanted = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, w, h).apply {
                setFeatureEnabled(MediaCodecInfo.CodecCapabilities.FEATURE_LowLatency, true)
            }
            MediaCodecList(MediaCodecList.REGULAR_CODECS).findDecoderForFormat(wanted)
        }.getOrNull()
        val decoder = if (lowLatency != null) MediaCodec.createByCodecName(lowLatency)
        else MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        Log.i(TAG, "decoder ${decoder.name} for ${w}x$h")
        try {
            decoder.configure(format, surface, null, 0)
            decoder.start()
        } catch (e: Exception) {
            decoder.release() // a failed decoder still holds a hardware instance
            throw e
        }
        return decoder
    }

    /** Hands data to the decoder; false if it had no room. */
    private fun queue(data: ByteArray, ptsUs: Long, flags: Int): Boolean {
        val c = codec ?: return false
        val index = c.dequeueInputBuffer(20_000)
        if (index < 0) return false // decoder saturated: drop (the next keyframe heals)
        val buf = c.getInputBuffer(index) ?: return false
        buf.clear()
        buf.put(data)
        c.queueInputBuffer(index, 0, data.size, ptsUs, flags)
        if (flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG == 0) queuedAt[ptsUs] = System.nanoTime()
        return true
    }

    /**
     * Renders every decoded frame immediately, waiting up to [waitUs] for
     * the first one.
     */
    private fun drain(waitUs: Long) {
        val c = codec ?: return
        var wait = waitUs
        while (true) {
            val index = c.dequeueOutputBuffer(info, wait)
            if (index == MediaCodec.INFO_TRY_AGAIN_LATER) return
            if (index < 0) continue // format or buffers changed: keep going
            c.releaseOutputBuffer(index, true)
            // presentationTimeUs carries the computer's capture time.
            session.framePresented(info.presentationTimeUs.toULong())
            queuedAt.remove(info.presentationTimeUs)?.let { t ->
                val ms = (System.nanoTime() - t) / 1e6
                decodeSum += ms; if (ms > decodeMax) decodeMax = ms
                if (++decodeCount == 120) {
                    Log.d(TAG, "decode %.1f ms avg, %.1f max over 120 frames".format(decodeSum / 120, decodeMax))
                    decodeCount = 0; decodeSum = 0.0; decodeMax = 0.0
                }
            }
            if (queuedAt.size > 64) queuedAt.clear()
            if (pending > 0) pending--
            wait = 0
        }
    }

    private fun release() {
        codec?.let { runCatching { it.stop() }; runCatching { it.release() } }
        codec = null
        pending = 0
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
