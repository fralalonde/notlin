open class Base {
    open fun open(): Int = 1
}
sealed class Shape {
    class Circle : Shape()
}
fun entry(args: Array<String>) {
    println(args.size)
}