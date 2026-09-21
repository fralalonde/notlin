import java.util.concurrent.atomic.AtomicInteger

class Lazy {
    val size: Int by lazy { compute() }
    fun compute(): Int = 42
}
class Counter {
    lateinit var name: String
    var count: Int = 0
        private set
}