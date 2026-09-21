fun sum(vararg xs: Int): Int {
    var t = 0
    for (x in xs) t += x
    return t
}
fun greet(name: String = "world"): String = "hi " + name
fun String.shout(): String = this.uppercase()
fun main() {
    print(sum(1, 2, 3))
    print(greet())
    print("hello".shout())
}