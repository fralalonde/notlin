interface Greeter {
    fun greet(): String
}
class Parent2 : Greeter {
    override fun greet(): String = "hi"
}
class Gen<T>(val x: T) : AutoCloseable {
    override fun close() {}
}