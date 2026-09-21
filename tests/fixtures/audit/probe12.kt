fun main() {
    try {
        throw Exception("boom")
    } catch (e: Exception) {
        println(e.message)
    }
}
