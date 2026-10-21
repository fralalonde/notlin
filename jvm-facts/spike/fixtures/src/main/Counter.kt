package fixture

/**
 * The "callee lives in our own sources" case: `Counter.add` is declared in this
 * file, called from Site.kt. Resolving it proves the session indexes more than
 * one file and that the call-site -> declaration edge works.
 */
class Counter {
    private val seen = mutableListOf<Int>()

    fun add(value: Int): String {
        seen.add(value)
        return "added:$value"
    }

    /** Nullable-return probe: proves the K2 type exposes nullability, not just a rendering. */
    fun find(key: String): String? = if (key.isEmpty()) null else key

    fun total(): Int {
        var sum = 0
        for (item in seen) sum += item
        return sum
    }
}
