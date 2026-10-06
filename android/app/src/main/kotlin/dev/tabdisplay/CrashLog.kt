package dev.tabdisplay

import java.io.File

/**
 * Crashes leave a report that reaches the computer: an uncaught exception (any thread) or a
 * panic in the native code writes crash-<time>.txt (what happened, and the app's last log
 * lines); the next connection sends it to the host, which prints and logs it, and deletes it.
 * (Hard native crashes, e.g. a segfault, are in Android's crash log, which the host saves.)
 */
object CrashLog {
    private var dir: File? = null

    fun install(context: android.content.Context) {
        val d = File(context.filesDir, "crashes").apply { mkdirs() }
        dir = d
        Native.crashFile(File(d, "native-crash.txt").path)
        val previous = Thread.getDefaultUncaughtExceptionHandler()
        Thread.setDefaultUncaughtExceptionHandler { thread, e ->
            try {
                File(d, "crash-${System.currentTimeMillis()}.txt").writeText(
                    "the app crashed on thread ${thread.name}:\n${e.stackTraceToString()}\n" +
                        "last log lines:\n${TLog.recent().joinToString("\n")}\n",
                )
            } catch (_: Throwable) {}
            previous?.uncaughtException(thread, e)
        }
    }

    /** Reports from earlier runs, oldest first, as log lines (then deleted). */
    fun sendPending() {
        val files = dir?.listFiles()?.filter { it.length() > 0 }?.sortedBy { it.lastModified() } ?: return
        for (f in files) {
            TLog.i("previous run: ${f.readText().take(12000)}")
            f.delete()
        }
    }
}
