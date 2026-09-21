class Account(val owner: String)
fun comparisons(x: Int, y: Int): Boolean {
    return x > y && x != y || x == y
}
fun structEq(a: Account, b: Account): Boolean {
    return a == b
}
