package g

fun <T : Comparable<T>> maxOf(a: T, b: T): T {
    return if (a > b) a else b
}
