class Callback(val fn: () -> Unit, val ints: List<Int>, val raw: IntArray)
fun test(cb: Callback) {
    cb.fn()
}