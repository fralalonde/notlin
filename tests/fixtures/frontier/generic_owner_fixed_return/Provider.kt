package probe
@Deprecated("retained metadata")
interface Provider<T> : Wide {
    val stored: Marker
    override fun read(): Marker = stored
}
