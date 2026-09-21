package com.example.demo

import java.util.List

class Person(val name: String, var age: Int) {
    val adult: Boolean
        get() = age >= 18

    fun greet(): String {
        return "Hello, $name"
    }

    fun haveBirthday() {
        age += 1
    }
}

data class Point(val x: Int, val y: Int)

object Registry {
    val items = mutableListOf<String>()

    fun register(item: String) {
        items.add(item)
    }
}

fun main() {
    val p = Person("Ada", 36)
    println(p.greet())
    p.haveBirthday()
    val pt = Point(1, 2)
    println(pt)
    Registry.register("x")
    for (i in 0..9) {
        println(i)
    }
}
