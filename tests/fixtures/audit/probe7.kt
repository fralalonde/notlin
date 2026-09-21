fun <reified T> typeName(): String = "T"
fun <T> identity(x: T): T = x
fun <T : Comparable<T>> maxOf(a: T, b: T): T = a
fun <T, R : Number> pair(x: T, y: R): T = x