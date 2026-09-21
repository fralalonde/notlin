// Expressions: operators, strings with interpolation, comparisons.
package com.example.expr

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

fun strings(user: String, count: Int): String {
    val simple = "hello"
    val interp = "user=$user count=$count"
    val braces = "$user has $count items"
    val escaped = "tab\there"
    return interp + braces + escaped + simple
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

fun safeCall(c: Account?): String {
    return c?.display() ?: "none"
}

fun cast(z: Any): Int {
    return if (z is Int) z else 0
}
