package probe;

import java.lang.reflect.Constructor;
import java.lang.reflect.Modifier;
import java.util.AbstractCollection;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collection;
import java.util.Iterator;
import java.util.List;

/**
 * Java-side oracle for the Bag constructor fixture.
 * Every check prints NAME=expected:actual:true|false and the process exits
 * non-zero if any expectation fails, so a silent divergence cannot pass.
 */
public final class JavaDriver {

    /** Log of observable collection operations, in call order. */
    static final List<String> LOG = new ArrayList<>();

    static void reset() {
        LOG.clear();
    }

    /** Custom collection recording exactly which bulk operations the callee performs. */
    static final class RecordingCollection extends AbstractCollection<Element> {
        private final Element[] items;

        RecordingCollection(Element... items) {
            this.items = items;
        }

        @Override
        public Iterator<Element> iterator() {
            LOG.add("iterator");
            return Arrays.asList(items).iterator();
        }

        @Override
        public int size() {
            LOG.add("size");
            return items.length;
        }

        @Override
        public Object[] toArray() {
            LOG.add("toArray");
            return items.clone();
        }
    }

    static int failures = 0;

    static void check(String name, String expected, String actual) {
        boolean ok = expected.equals(actual);
        if (!ok) {
            failures++;
        }
        System.out.println(name + "=" + expected + ":" + actual + ":" + ok);
    }

    static void checkTrue(String name, boolean actual) {
        check(name, "true", Boolean.toString(actual));
    }

    static <T extends Throwable> void expectThrows(String name, Class<T> type, Runnable body) {
        String seen = "none";
        try {
            body.run();
        } catch (Throwable t) {
            seen = t.getClass().getName();
        }
        check(name, type.getName(), seen);
    }

    static String ids(Collection<Element> c) {
        StringBuilder sb = new StringBuilder();
        for (Element e : c) {
            if (sb.length() > 0) {
                sb.append(',');
            }
            sb.append(e == null ? "null" : e.id);
        }
        return sb.toString();
    }

    public static void main(String[] args) {
        Element a = new Element("a");
        Element b = new Element("b");
        Element c = new Element("c");

        // ---- construction form 1: no-argument ----
        reset();
        Bag empty = new Bag();
        check("form1.emptySize", "0", Integer.toString(empty.size()));
        check("form1.log", "[]", LOG.toString());

        // ---- construction form 2: vararg elements ----
        reset();
        Bag varargs = new Bag(a, b, c);
        check("form2.size", "3", Integer.toString(varargs.size()));
        check("form2.order", "a,b,c", ids(varargs));
        checkTrue("form2.identityOnce", varargs.get(0) == a && varargs.get(1) == b && varargs.get(2) == c);
        check("form2.superclassLog", "[]", LOG.toString());

        // ---- construction form 3: Collection<Element> ----
        reset();
        Bag fromList = new Bag(List.of(a, b));
        check("form3.size", "2", Integer.toString(fromList.size()));
        check("form3.order", "a,b", ids(fromList));

        // ---- bulk-copy effect: exactly one toArray, in order, once ----
        reset();
        Element x = new Element("x");
        Element y = new Element("y");
        Bag fromRecording = new Bag(new RecordingCollection(x, y));
        check("bulk.exactOps", "[toArray]", LOG.toString());
        check("bulk.size", "2", Integer.toString(fromRecording.size()));
        check("bulk.order", "x,y", ids(fromRecording));
        checkTrue("bulk.identityOnce",
                fromRecording.get(0) == x && fromRecording.get(1) == y
                        && fromRecording.stream().filter(e -> e == x).count() == 1);

        // ---- null container rejected ----
        expectThrows("null.collection", NullPointerException.class,
                () -> new Bag((Collection<Element>) null));
        expectThrows("null.varargArray", NullPointerException.class,
                () -> new Bag((Element[]) null));

        // ---- null element inside the array is allowed ----
        reset();
        String elemNull = "none";
        Bag withNullElement = null;
        try {
            withNullElement = new Bag(new Element[] {null, a});
            elemNull = "ok";
        } catch (Throwable t) {
            elemNull = t.getClass().getName();
        }
        check("nullElementInArray", "ok", elemNull);
        if (withNullElement != null) {
            check("nullElementInArray.size", "2", Integer.toString(withNullElement.size()));
            check("nullElementInArray.order", "null,a", ids(withNullElement));
        }

        // single null element via vararg (not a null array)
        String singleNull = "none";
        int singleNullSize = -1;
        try {
            Bag one = new Bag((Element) null);
            singleNull = "ok";
            singleNullSize = one.size();
        } catch (Throwable t) {
            singleNull = t.getClass().getName();
        }
        check("singleNullElement", "ok", singleNull);
        check("singleNullElement.size", "1", Integer.toString(singleNullSize));

        // ---- reflective ABI record ----
        System.out.println("REFLECT class.name=" + Bag.class.getName());
        System.out.println("REFLECT class.modifiers=" + Modifier.toString(Bag.class.getModifiers())
                + " raw=0x" + Integer.toHexString(Bag.class.getModifiers()));
        System.out.println("REFLECT class.superclass=" + Bag.class.getSuperclass().getName());
        System.out.println("REFLECT class.interfaces="
                + Arrays.toString(Arrays.stream(Bag.class.getInterfaces()).map(Class::getName).toArray()));
        System.out.println("REFLECT marker.isInterface=" + Marker.class.isInterface()
                + " methods=" + Marker.class.getDeclaredMethods().length);
        System.out.println("REFLECT element.classModifiers=" + Modifier.toString(Element.class.getModifiers()));

        Constructor<?>[] ctors = Bag.class.getDeclaredConstructors();
        Arrays.sort(ctors, (l, r) -> l.toGenericString().compareTo(r.toGenericString()));
        System.out.println("REFLECT ctor.count=" + ctors.length);
        for (Constructor<?> ctor : ctors) {
            System.out.println("REFLECT ctor[" + ctor.toGenericString() + "]"
                    + " modifiers=" + Modifier.toString(ctor.getModifiers())
                    + " raw=0x" + Integer.toHexString(ctor.getModifiers())
                    + " varargs=" + ctor.isVarArgs()
                    + " synthetic=" + ctor.isSynthetic()
                    + " params=" + Arrays.toString(ctor.getParameterTypes())
                    + " genericParams=" + Arrays.toString(ctor.getGenericParameterTypes())
                    + " throws=" + Arrays.toString(ctor.getExceptionTypes()));
        }

        System.out.println("FAILURES=" + failures);
        if (failures != 0) {
            System.exit(1);
        }
        System.out.println("ALL_CHECKS_PASSED");
    }
}
