package dev.tabdisplay

/**
 * LZ4 block decompression (the raw block format, as `lz4_flex::block::compress` writes it).
 * Decodes [len] bytes of [src] from [off] into [dst]; returns the number of bytes written.
 */
fun lz4Decompress(src: ByteArray, off: Int, len: Int, dst: ByteArray): Int {
    var s = off
    val end = off + len
    var d = 0
    while (s < end) {
        val token = src[s++].toInt() and 0xff
        var literals = token ushr 4
        if (literals == 15) {
            while (true) {
                val b = src[s++].toInt() and 0xff
                literals += b
                if (b != 255) break
            }
        }
        System.arraycopy(src, s, dst, d, literals)
        s += literals
        d += literals
        if (s >= end) break // the last sequence has literals only
        val offset = (src[s].toInt() and 0xff) or ((src[s + 1].toInt() and 0xff) shl 8)
        s += 2
        var match = token and 15
        if (match == 15) {
            while (true) {
                val b = src[s++].toInt() and 0xff
                match += b
                if (b != 255) break
            }
        }
        match += 4
        var m = d - offset
        if (offset >= match) {
            System.arraycopy(dst, m, dst, d, match)
            d += match
        } else {
            repeat(match) { dst[d++] = dst[m++] } // overlapping copy repeats the pattern
        }
    }
    return d
}
