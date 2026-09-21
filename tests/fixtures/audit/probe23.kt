fun math(a: Int, b: Int): Int {
    val sum = a + b
    val diff = a - b
    return sum + diff
}
fun structEq(a: Account, b: Account): Boolean {
    return a == b
}
class Account(val owner: String)
