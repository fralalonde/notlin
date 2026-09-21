interface Clock {
    fun now(): Long
}
object TheClock : Clock {
    override fun now(): Long = 1L
}
class Box : Clock {
    override fun now(): Long = 2L
}
fun main() {
    val pairs = listOf(1 to 2)
    for ((a, b) in pairs) {
        println(a + b)
    }
    val (x, y) = 5 to 6
    println(x + y)
    val arr = intArrayOf(1, 2, 3)
    println(arr[0])
    val n = arr.size
    val whenVal = when (n) {
        0 -> "zero"
        else -> "many"
    }
    println(whenVal)
}