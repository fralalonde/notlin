package s

value class NoJava(val raw: Int)

typealias Handler = (Int) -> String

fun <T : Any> reify(v: T): String {
    return v.toString()
}
