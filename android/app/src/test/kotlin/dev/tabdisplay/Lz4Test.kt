package dev.tabdisplay

import org.junit.Assert.assertArrayEquals
import org.junit.Test

class Lz4Test {
    private fun hex(s: String) = ByteArray(s.length / 2) { s.substring(2 * it, 2 * it + 2).toInt(16).toByte() }

    /** Vectors written by the host's `lz4_vectors` test (lz4_flex::block::compress). */
    @Test
    fun decodesHostOutput() {
        val lines = javaClass.classLoader!!.getResource("lz4_vectors.txt").readText().lines()
        for (line in lines) {
            val parts = line.split(" ")
            val raw = hex(parts[0])
            val packed = hex(parts.getOrElse(1) { "" })
            val out = ByteArray(raw.size)
            val n = lz4Decompress(packed, 0, packed.size, out)
            assertArrayEquals(raw, out.copyOf(n))
        }
    }
}
