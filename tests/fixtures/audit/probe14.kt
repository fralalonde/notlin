interface HasLabel {
    val label: String
        get() = "default"
    val size: Int
}
fun classify(x: Int): String {
    return when (x) {
        0, 1 -> "small"
        in 2..9 -> "medium"
        else -> "large"
    }
}