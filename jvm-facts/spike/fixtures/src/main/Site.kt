package fixture

/**
 * Call sites under test. Two `add` calls, deliberately different owners: one on
 * a user-declared class (Counter.kt), one on a stdlib generic (MutableList).
 * A Kotlin provider that only answers the second is not done; one that only
 * answers the first has not proven stdlib resolution.
 */
fun useCounter(counter: Counter, value: Int): String {
    val rendered: String = counter.add(value)
    return rendered
}

fun grow(list: MutableList<Int>, value: Int) {
    list.add(value)
}

fun useFind(counter: Counter): String? {
    val hit: String? = counter.find("k")
    return hit
}
