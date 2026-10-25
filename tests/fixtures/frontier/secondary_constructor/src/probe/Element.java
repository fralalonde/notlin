package probe;

/** Fully generic element value; deliberately member-free so the fixture
 *  isolates constructor/delegation semantics, not element behavior. */
public class Element {
    public final String id;

    public Element(String id) {
        this.id = id;
    }

    @Override
    public String toString() {
        return "Element(" + id + ")";
    }
}
