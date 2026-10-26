package probe;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.util.Arrays;
import java.util.TreeSet;

public final class Driver {
    public static void main(String[] args) {
        Marker marker = new Marker("kept");
        Subject subject = new Subject(marker);
        Wide wide = subject;
        System.out.println(wide.read() == marker);
        System.out.println(subject.read() == marker);
        System.out.println(subject.getStored() == marker);
        try {
            new Subject(null);
            System.out.println("NULL_ACCEPTED");
        } catch (NullPointerException expected) {
            System.out.println("NPE");
        }
        TreeSet<String> api = new TreeSet<>();
        for (Method method : Subject.class.getDeclaredMethods()) {
            api.add(method.getName() + Arrays.toString(method.getParameterTypes())
                + ":" + method.getReturnType().getName()
                + ":" + Modifier.toString(method.getModifiers())
                + ":bridge=" + method.isBridge()
                + ":synthetic=" + method.isSynthetic());
        }
        for (String line : api) System.out.println(line);
    }
}
