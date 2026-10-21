package notlin.facts;

import java.io.File;
import java.lang.annotation.Annotation;
import java.lang.reflect.AnnotatedElement;
import java.lang.reflect.Constructor;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.net.MalformedURLException;
import java.net.URL;
import java.net.URLClassLoader;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Set;

/**
 * Reflection source of Java classpath facts.
 *
 * <p>Answers {@code ClassMembers} for a binary class name using only the JDK:
 * the platform loader resolves {@code java.*}/{@code javax.*} without help, and
 * user jars come from {@code NOTLIN_CLASSPATH} (colon-separated) through a
 * {@link URLClassLoader}. Classes are loaded with {@code initialize=false} so
 * querying a type never runs its static initializers.
 *
 * <p>Facts are public-only and fail closed: a type that cannot be loaded, is not
 * public, or whose member enumeration raises a linkage error yields
 * {@code members: null} ("unknown"), never a partial or empty list. Nullability
 * comes from runtime-visible annotations only; anything else is {@code unknown}.
 */
public final class Facts {

    static final int PROTOCOL_VERSION = 1;

    /** Runtime-visible annotations that establish nullability; nothing is inferred. */
    static final String NOT_NULL = "org.jetbrains.annotations.NotNull";
    static final String NULLABLE = "org.jetbrains.annotations.Nullable";

    /** Binary names only: anything else is rejected before it reaches the loader. */
    private static final String OWNER_PATTERN = "[A-Za-z0-9_$.]+";

    private static final Comparator<Member> MEMBER_ORDER =
            Comparator.<Member>comparingInt(member -> member.name().equals("<init>") ? 0 : 1)
                    .thenComparing(Member::name)
                    .thenComparing(member -> String.join(",", member.parameterTypes()));

    private final ClassLoader loader;

    private Facts(ClassLoader loader) {
        this.loader = loader;
    }

    /** Builds the loader once: {@code NOTLIN_CLASSPATH} jars/dirs over the platform loader. */
    public static Facts create() {
        String configured = System.getenv("NOTLIN_CLASSPATH");
        List<URL> urls = new ArrayList<>();
        if (configured != null) {
            for (String entry : configured.split(":")) {
                String trimmed = entry.trim();
                if (trimmed.isEmpty()) {
                    continue;
                }
                try {
                    urls.add(new File(trimmed).toURI().toURL());
                } catch (MalformedURLException malformed) {
                    warn("ignoring classpath entry " + trimmed + ": " + malformed.getMessage());
                }
            }
        }
        if (urls.isEmpty()) {
            return new Facts(ClassLoader.getSystemClassLoader());
        }
        return new Facts(new URLClassLoader(urls.toArray(new URL[0]), ClassLoader.getSystemClassLoader()));
    }

    /** One member row, mirroring {@code MemberFacts} on the Rust side. */
    public record Member(
            String name,
            boolean staticMember,
            List<String> parameterTypes,
            String resultType,
            List<String> parameterNullability,
            String resultNullability) {
    }

    /**
     * Public declared constructors plus public declared-and-inherited methods of
     * {@code owner}, or {@code null} when the type cannot be established.
     */
    public List<Member> members(String owner) {
        if (owner == null || !owner.matches(OWNER_PATTERN)) {
            warn("rejecting owner outside [A-Za-z0-9_$.]: " + owner);
            return null;
        }
        Class<?> type;
        try {
            type = Class.forName(owner, false, loader);
        } catch (ClassNotFoundException | LinkageError missing) {
            warn("cannot load " + owner + ": " + missing);
            return null;
        } catch (RuntimeException failure) {
            warn("cannot load " + owner + ": " + failure);
            return null;
        }
        if (!Modifier.isPublic(type.getModifiers())) {
            warn("owner is not public: " + owner);
            return null;
        }
        List<Member> members = new ArrayList<>();
        Set<String> seen = new LinkedHashSet<>();
        try {
            for (Constructor<?> constructor : type.getDeclaredConstructors()) {
                int modifiers = constructor.getModifiers();
                if (!Modifier.isPublic(modifiers) || constructor.isSynthetic()) {
                    continue;
                }
                members.add(row("<init>", modifiers, constructor.getParameterTypes(), void.class,
                        constructor.getParameterAnnotations(), constructor));
            }
            for (Class<?> current = type; current != null && current != Object.class;
                    current = current.getSuperclass()) {
                for (Method method : current.getDeclaredMethods()) {
                    int modifiers = method.getModifiers();
                    if (!Modifier.isPublic(modifiers) || method.isSynthetic() || method.isBridge()) {
                        continue;
                    }
                    if (!seen.add(method.getName() + descriptor(method.getParameterTypes()))) {
                        continue;
                    }
                    members.add(row(method.getName(), modifiers, method.getParameterTypes(),
                            method.getReturnType(), method.getParameterAnnotations(), method));
                }
            }
        } catch (LinkageError | RuntimeException failure) {
            // A partially enumerated class is unknown, not empty: fail closed.
            warn("incomplete members for " + owner + ": " + failure);
            return null;
        }
        members.sort(MEMBER_ORDER);
        return members;
    }

    /** Serializes the {@code Members} answer line for one request id. */
    public String membersAnswer(long id, String owner) {
        List<Member> resolved;
        try {
            resolved = members(owner);
        } catch (LinkageError | RuntimeException failure) {
            warn("members(" + owner + ") failed: " + failure);
            resolved = null;
        }
        StringBuilder line = new StringBuilder(256);
        line.append("{\"version\":").append(PROTOCOL_VERSION)
                .append(",\"id\":").append(id)
                .append(",\"answer\":{\"Members\":{\"owner\":");
        appendString(line, owner);
        line.append(",\"members\":");
        if (resolved == null) {
            line.append("null");
        } else {
            line.append('[');
            for (int index = 0; index < resolved.size(); index++) {
                if (index > 0) {
                    line.append(',');
                }
                appendMember(line, resolved.get(index));
            }
            line.append(']');
        }
        line.append("}}}");
        return line.toString();
    }

    private static Member row(String name, int modifiers, Class<?>[] parameters, Class<?> result,
            Annotation[][] parameterAnnotations, AnnotatedElement element) {
        List<String> parameterTypes = new ArrayList<>(parameters.length);
        List<String> parameterNullability = new ArrayList<>(parameters.length);
        for (int index = 0; index < parameters.length; index++) {
            parameterTypes.add(parameters[index].getName());
            parameterNullability.add(nullability(parameterAnnotations[index]));
        }
        return new Member(
                name,
                Modifier.isStatic(modifiers),
                List.copyOf(parameterTypes),
                result == void.class ? "<void>" : result.getName(),
                List.copyOf(parameterNullability),
                nullability(element.getAnnotations()));
    }

    /** {@code non_null} / {@code nullable} only when proven; otherwise {@code unknown}. */
    static String nullability(Annotation[] annotations) {
        for (Annotation annotation : annotations) {
            String type = annotation.annotationType().getName();
            if (NOT_NULL.equals(type)) {
                return "non_null";
            }
            if (NULLABLE.equals(type)) {
                return "nullable";
            }
        }
        return "unknown";
    }

    private static String descriptor(Class<?>[] parameters) {
        StringBuilder descriptor = new StringBuilder("(");
        for (Class<?> parameter : parameters) {
            descriptor.append(parameter.getName()).append(',');
        }
        return descriptor.append(')').toString();
    }

    private static void appendMember(StringBuilder out, Member member) {
        out.append("{\"name\":");
        appendString(out, member.name());
        out.append(",\"static_member\":").append(member.staticMember());
        out.append(",\"parameter_types\":");
        appendStrings(out, member.parameterTypes());
        out.append(",\"result_type\":");
        appendString(out, member.resultType());
        out.append(",\"parameter_nullability\":");
        appendStrings(out, member.parameterNullability());
        out.append(",\"result_nullability\":");
        appendString(out, member.resultNullability());
        out.append('}');
    }

    private static void appendStrings(StringBuilder out, List<String> values) {
        out.append('[');
        for (int index = 0; index < values.size(); index++) {
            if (index > 0) {
                out.append(',');
            }
            appendString(out, values.get(index));
        }
        out.append(']');
    }

    private static void appendString(StringBuilder out, String value) {
        out.append('"');
        for (int index = 0; index < value.length(); index++) {
            char current = value.charAt(index);
            switch (current) {
                case '"' -> out.append("\\\"");
                case '\\' -> out.append("\\\\");
                case '\n' -> out.append("\\n");
                case '\r' -> out.append("\\r");
                case '\t' -> out.append("\\t");
                default -> {
                    if (current < 0x20) {
                        out.append(String.format("\\u%04x", (int) current));
                    } else {
                        out.append(current);
                    }
                }
            }
        }
        out.append('"');
    }

    static void warn(String message) {
        System.err.println("jvm-facts: " + message);
    }
}
