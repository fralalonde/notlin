package probe;

// LABELLED MANUAL EQUIVALENT — hand-written probe artifact, NOT emitter output.
// Purpose: measure the ABI delta a hypothetical faithful emitter would have to close.
public final class Bag extends java.util.ArrayList<Element> implements Marker {
    public Bag() {
        super();
    }

    public Bag(java.util.Collection<? extends Element> elements) {
        super();
        addAll(elements);
    }

    public Bag(Element... elements) {
        super();
        addAll(java.util.Arrays.asList(elements));
    }
}
