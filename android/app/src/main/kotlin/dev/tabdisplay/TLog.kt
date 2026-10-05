package dev.tabdisplay

/**
 * Log lines that explain what the tablet does (renderer, decoder, failures): to logcat and to
 * the host, which prints them as "tablet: ...", so nobody needs adb to see them. Lines from
 * before a connection are kept and sent once one is up.
 */
object TLog {
    @Volatile private var sink: ((String) -> Unit)? = null
    private val early = ArrayDeque<String>()

    fun i(msg: String) {
        android.util.Log.i("tabdisplay", msg)
        val s = sink
        if (s != null) s(msg) else synchronized(early) {
            if (early.size >= 40) early.removeFirst()
            early.addLast(msg)
        }
    }

    /** A connection is up: what was logged meanwhile goes first. */
    fun connected(send: (String) -> Unit) {
        synchronized(early) {
            early.forEach(send)
            early.clear()
            sink = send
        }
    }

    fun disconnected() {
        sink = null
    }
}
