data class Point(val x: Int, val y: Int) : Runnable {
    override fun run() {}
}
data class Named(val name: String) : AutoCloseable {
    override fun close() {}
}