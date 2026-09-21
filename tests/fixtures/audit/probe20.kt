fun strings(user: String, count: Int): String {
    val interp = "user=$user count=$count"
    return interp
}
class A {}
fun eq(a: A, b: A): Boolean { return a == b }
