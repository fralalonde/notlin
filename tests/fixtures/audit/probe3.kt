class Parent
class Child : Parent()
interface Greeter {
    fun greet(): String
}
class Widget : Greeter by Parent2()
class Parent2 : Greeter {
    override fun greet(): String = "hi"
}