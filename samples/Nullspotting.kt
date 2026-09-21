// Nullability annotated, not enforced: `T?` emits @Nullable on the Java side.
package com.example.nulls

class Profile(var name: String) {
    var nickname: String? = null
    var score: Int? = null

    fun display(): String = nickname ?: name

    fun lookup(id: Int?): Profile? {
        return this
    }
}

fun safeLen(s: String?): Int {
    return s?.length ?: 0
}
