package probe

// Control only: isolate constructor signature and bare-supertype emission.
class Bag : ArrayList<Element>, Marker {
    constructor() : super()
    constructor(values: Collection<Element>) : super()
    constructor(vararg values: Element) : super()
}
