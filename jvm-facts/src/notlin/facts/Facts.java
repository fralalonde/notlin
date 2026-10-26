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
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.Deque;
import java.util.HashSet;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Set;

/**
 * Reflection source of Java classpath facts.
 *
 * <p>Answers {@code ClassMembers} for a binary class name using only the JDK:
 * the platform loader resolves {@code java.*}/{@code javax.*} without help, and
 * user jars/directories come from {@code NOTLIN_CLASSPATH} (colon-separated)
 * through a {@link URLClassLoader}. Classes are loaded with
 * {@code initialize=false} so querying a type never runs its static initializers.
 * Kotlin file facades ({@code <Name>Kt} classes holding top-level and extension
 * functions as statics) need no special handling: they are ordinary classes once
 * the jar that contains them is on that classpath.
 *
 * <p>Members are the public constructors declared by the owner plus the public
 * methods of the owner <em>and its whole public supertype graph</em> — superclass
 * chain first, then every interface transitively. That is what makes an interface
 * or abstract owner usable: {@code java.util.SortedSet} declares neither
 * {@code size} nor {@code add}, both of which callers on that owner need. Each
 * {@code (name, parameter types)} pair appears once, most-derived declaration
 * first, so a covariant override never duplicates its parent.
 *
 * <p>Facts are public-only and fail closed: a type that cannot be loaded, is not
 * public, or whose member enumeration raises a linkage error yields
 * {@code members: null} ("unknown"), never a partial or empty list. Nullability
 * comes from class-file annotation attributes first (jetbrains annotations are
 * {@code CLASS}-retained, invisible to reflection) and from runtime-visible
 * annotations second; anything else is {@code unknown}.
 */
public final class Facts {

    static final int PROTOCOL_VERSION = 1;

    /** Binary names only: anything else is rejected before it reaches the loader. */
    private static final String OWNER_PATTERN = "[A-Za-z0-9_$.]+";

    private static final Comparator<Member> MEMBER_ORDER =
            Comparator.<Member>comparingInt(member -> member.name().equals("<init>") ? 0 : 1)
                    .thenComparing(Member::name)
                    .thenComparing(member -> String.join(",", member.parameterTypes()));

    private final ClassLoader loader;
    /** {@code null} when the running JDK has no {@code java.lang.classfile} API (pre-24). */
    private final ClassFileNullability classFiles;

    private Facts(ClassLoader loader) {
        this.loader = loader;
        this.classFiles = classFileReader(loader);
    }

    /**
     * Class-file nullability is enrichment, not a requirement: when the running
     * JDK cannot link {@code java.lang.classfile} the helper keeps answering
     * members with {@code unknown} nullability instead of failing. The reference
     * is confined to this factory so no other method links against that API.
     */
    private static ClassFileNullability classFileReader(ClassLoader loader) {
        try {
            return new ClassFileNullability(loader);
        } catch (LinkageError | RuntimeException unavailable) {
            warn("class-file nullability unavailable: " + unavailable);
            return null;
        }
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
            String resultNullability,
            List<String> checkedExceptions) {
    }

    /**
     * Public declared constructors plus public declared-and-inherited methods of
     * {@code owner} and its supertypes, or {@code null} when the type cannot be
     * established.
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
                members.add(row(type, "<init>", modifiers, constructor.getParameterTypes(), void.class,
                        constructor.getParameterAnnotations(), constructor,
                        constructor.getExceptionTypes()));
            }
            for (Class<?> current : hierarchy(type)) {
                for (Method method : current.getDeclaredMethods()) {
                    int modifiers = method.getModifiers();
                    if (!Modifier.isPublic(modifiers) || method.isSynthetic() || method.isBridge()) {
                        continue;
                    }
                    // Interface statics are not inherited by subtypes, and only the
                    // owner's own statics belong to a ClassMembers(owner) answer.
                    if (current != type && current.isInterface() && Modifier.isStatic(modifiers)) {
                        continue;
                    }
                    if (!seen.add(method.getName() + descriptor(method.getParameterTypes()))) {
                        continue;
                    }
                    members.add(row(current, method.getName(), modifiers, method.getParameterTypes(),
                            method.getReturnType(), method.getParameterAnnotations(), method,
                            method.getExceptionTypes()));
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

    /**
     * Supertypes of {@code owner} in declaration-priority order: the class chain
     * most-derived first (stopping before {@code Object}, whose methods are noise
     * for call resolution), then interfaces breadth-first. Class and abstract-class
     * declarations therefore always win over interface declarations, matching JVM
     * method resolution, and each type is visited once even when reached through
     * several paths.
     */
    private static List<Class<?>> hierarchy(Class<?> owner) {
        List<Class<?>> order = new ArrayList<>();
        for (Class<?> current = owner; current != null && current != Object.class;
                current = current.getSuperclass()) {
            order.add(current);
        }
        Set<Class<?>> visited = new HashSet<>(order);
        Deque<Class<?>> pending = new ArrayDeque<>();
        for (Class<?> current : order) {
            for (Class<?> implemented : current.getInterfaces()) {
                pending.addLast(implemented);
            }
        }
        while (!pending.isEmpty()) {
            Class<?> implemented = pending.removeFirst();
            if (implemented == null || !visited.add(implemented)) {
                continue;
            }
            order.add(implemented);
            for (Class<?> parent : implemented.getInterfaces()) {
                pending.addLast(parent);
            }
        }
        return order;
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

    /**
     * Serializes the per-request <em>absence</em> answer for one request id.
     *
     * <p>The helper was asked and is healthy, but has nothing to say about this
     * one request (it did not index that file, cannot read it, or failed to
     * analyse it). That is a fact the parent must be able to receive: the Rust
     * side waits for exactly one line per request, so silence is
     * indistinguishable from a stalled helper and blocks the run on its own
     * stdout pipe. It is also not an empty batch, which would claim "looked,
     * saw no calls".
     *
     * <p>Wire shape (pinned by the Rust {@code Answer::NoEvidence} variant and
     * its byte-exact tests):
     * <pre>
     * {"version":1,"id":N,"answer":{"NoEvidence":{"query":"ResolveCalls","reason":"..."}}}
     * </pre>
     *
     * <p>{@code reason} is diagnostic only; the Rust side never parses it, and
     * it deliberately carries no paths.
     */
    public static String noEvidenceAnswer(long id, String query, String reason) {
        StringBuilder line = new StringBuilder(128);
        line.append("{\"version\":").append(PROTOCOL_VERSION)
                .append(",\"id\":").append(id)
                .append(",\"answer\":{\"NoEvidence\":{\"query\":");
        appendString(line, query);
        line.append(",\"reason\":");
        appendString(line, reason);
        line.append("}}}");
        return line.toString();
    }

    private Member row(Class<?> declaring, String name, int modifiers, Class<?>[] parameters,
            Class<?> result, Annotation[][] parameterAnnotations, AnnotatedElement element,
            Class<?>[] exceptions) {
        String[] fromClassFile = classFiles == null ? null
                : classFiles.nullability(declaring, name, parameters, result);
        List<String> parameterTypes = new ArrayList<>(parameters.length);
        List<String> parameterNullability = new ArrayList<>(parameters.length);
        for (int index = 0; index < parameters.length; index++) {
            parameterTypes.add(parameters[index].getName());
            String nullability = fromClassFile == null
                    ? ClassFileNullability.UNKNOWN
                    : fromClassFile[index];
            if (ClassFileNullability.UNKNOWN.equals(nullability)) {
                nullability = nullability(parameterAnnotations[index]);
            }
            parameterNullability.add(nullability);
        }
        String resultNullability = fromClassFile == null
                ? ClassFileNullability.UNKNOWN
                : fromClassFile[parameters.length];
        if (ClassFileNullability.UNKNOWN.equals(resultNullability)) {
            resultNullability = nullability(element.getAnnotations());
        }
        return new Member(
                name,
                Modifier.isStatic(modifiers),
                List.copyOf(parameterTypes),
                result == void.class ? "<void>" : result.getName(),
                List.copyOf(parameterNullability),
                resultNullability,
                checkedExceptions(exceptions));
    }

    /**
     * Checked exception binary names of one physical declaration.
     *
     * <p>An exception type is checked unless it is assignable to
     * {@link RuntimeException} or {@link Error}. Anything the helper cannot
     * classify is treated as checked: that answer is the conservative one for
     * the only consumer (a Java call the emitter may write verbatim, which has
     * no {@code throws}/{@code catch} model). This never fabricates an empty
     * list: an exception type that cannot be inspected stays in the list.
     *
     * <p>If the enumeration itself raises a linkage error, {@link #members}
     * aborts and answers {@code members: null} — whole-owner Unknown — rather
     * than returning rows with an invented empty throws list.
     */
    private static List<String> checkedExceptions(Class<?>[] exceptions) {
        List<String> checked = new ArrayList<>(exceptions.length);
        for (Class<?> exception : exceptions) {
            boolean unchecked;
            try {
                unchecked = RuntimeException.class.isAssignableFrom(exception)
                        || Error.class.isAssignableFrom(exception);
            } catch (LinkageError | RuntimeException cannotClassify) {
                unchecked = false;
            }
            if (!unchecked) {
                checked.add(exception.getName());
            }
        }
        return List.copyOf(checked);
    }

    /** {@code non_null} / {@code nullable} only when proven; otherwise {@code unknown}. */
    static String nullability(Annotation[] annotations) {
        for (Annotation annotation : annotations) {
            String type = annotation.annotationType().getName();
            if (ClassFileNullability.NOT_NULL.equals(type)) {
                return "non_null";
            }
            if (ClassFileNullability.NULLABLE.equals(type)) {
                return "nullable";
            }
        }
        return ClassFileNullability.UNKNOWN;
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
        out.append(",\"checked_exceptions\":");
        appendStrings(out, member.checkedExceptions());
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
