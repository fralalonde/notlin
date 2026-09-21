// Untranslatable constructs — with default (warn) mode these produce warnings
// and best-effort output; --untranslatable=error makes them exit non-zero.

package com.example.demos

// data class with custom methods stays a record
data class Currency(val code: String, val rate: Double) {
    fun convert(amount: Int): Double = amount * rate

    val display: String
        get() = code + " @ " + rate
}

// inline value class — no Java counterpart, approximated
value class UserId(val raw: Int)

interface Shape {
    val name: String
    fun area(): Double
}

class Circle(val r: Double) : Shape {
    override val name: String
        get() = "circle"
    override fun area(): Double = 3.14159 * r * r
}

fun useWhen(x: Int): String {
    return when (x) {
        0 -> "zero"
        1 -> "one"
        else -> "many"
    }
}

fun useElvis(name: String?): String {
    return name ?: "anonymous"
}

fun useSafeCall(p: Person2?): String {
    return p?.name ?: "unnamed"
}

fun ranges() {
    for (i in 10 downTo 0 step 2) {
        println(i)
    }
    for (j in 0 until 5) {
        println(j)
    }
}

fun listOfOps() {
    val xs = listOf(1, 2, 3)
    val doubled = xs.map { it * 2 }
    println(doubled)
}

class Person2(var name: String) {
    private var nickname: String? = null

    fun setNickname(n: String) {
        nickname = n
    }
}
