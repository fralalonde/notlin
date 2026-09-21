class Account(val owner: String) {
    fun display(): String = "acct($owner)"
}

fun math(a: Int, b: Int): Int {
    val sum = a + b
    val diff = a - b
    val prod = a * b
    val quot = a / b
    return sum + diff - quot
}

fun comparisons(x: Int, y: Int): Boolean {
    return x > y && x != y || x == y
}

fun structEq(a: Account, b: Account): Boolean {
    return a == b
}

fun elvis(s: String?): Int {
    return s?.length ?: 0
}