package probe

/**
 * Faithful sanitized shape of the selected constructor root:
 *  - class extends a generic Java collection (ArrayList<Element>)
 *  - and an already-Java, empty, annotation-free marker interface
 *  - implicit/default primary constructor (supertype written without ())
 *  - three secondary constructors, all delegating to super(...)
 *  - no properties, no overrides, no nullables, no annotations, no generics on the class
 */
class Bag : ArrayList<Element>, Marker {
    constructor() : super()

    constructor(elements: Collection<Element>) : super() {
        addAll(elements)
    }

    constructor(vararg elements: Element) : super() {
        addAll(elements.toList())
    }
}
