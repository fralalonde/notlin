package notlin.facts;

import java.io.IOException;
import java.io.InputStream;
import java.lang.classfile.Annotation;
import java.lang.classfile.ClassFile;
import java.lang.classfile.ClassModel;
import java.lang.classfile.MethodElement;
import java.lang.classfile.MethodModel;
import java.lang.classfile.attribute.RuntimeInvisibleAnnotationsAttribute;
import java.lang.classfile.attribute.RuntimeInvisibleParameterAnnotationsAttribute;
import java.lang.classfile.attribute.RuntimeVisibleAnnotationsAttribute;
import java.lang.classfile.attribute.RuntimeVisibleParameterAnnotationsAttribute;
import java.lang.invoke.MethodType;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

/**
 * Reads jetbrains {@code @NotNull}/{@code @Nullable} from class files.
 *
 * <p>Both annotations are declared with {@code RetentionPolicy.CLASS}, so
 * {@code java.lang.reflect} never sees them: they live in the
 * {@code RuntimeInvisibleAnnotations} / {@code RuntimeInvisibleParameterAnnotations}
 * attributes. The JDK's public {@code java.lang.classfile} API (final since JDK
 * 24, no preview flag) lets the helper read those attributes from the raw class
 * bytes the class loader serves for {@code <binary/name>.class}.
 *
 * <p>This class is an enrichment source only. When a class file cannot be read,
 * cannot be parsed, or simply carries no annotation, the answer is {@code unknown}
 * for every member of that class and the caller keeps its reflection fallback.
 * Nothing here can make a query fail: worst case nullability stays unknown.
 */
final class ClassFileNullability {

    static final String NOT_NULL = "org.jetbrains.annotations.NotNull";
    static final String NULLABLE = "org.jetbrains.annotations.Nullable";
    static final String UNKNOWN = "unknown";

    /** Nullability of one method or constructor; parameters may be shorter than the arity. */
    private record MethodNullability(String result, List<String> parameters) {
    }

    private final ClassLoader loader;

    /** Binary name to {@code name + descriptor} to nullability; empty map means "no answer". */
    private final Map<String, Map<String, MethodNullability>> cache = new HashMap<>();

    ClassFileNullability(ClassLoader loader) {
        this.loader = loader;
    }

    /**
     * Nullability for one member as {@code [param0 .. paramN-1, result]}, or
     * {@code null} when this class file carries no answer. Keyed by the exact JVM
     * descriptor so overloads and covariant overrides never collide. Returning a
     * {@code String[]} rather than a nested type keeps the caller free of any
     * linkage to the {@code java.lang.classfile} API.
     */
    String[] nullability(Class<?> declaring, String name, Class<?>[] parameters, Class<?> result) {
        String key;
        try {
            key = name + MethodType.methodType(result, parameters).toMethodDescriptorString();
        } catch (RuntimeException unmappable) {
            return null;
        }
        MethodNullability found = methodsOf(declaring).get(key);
        if (found == null) {
            return null;
        }
        String[] slots = new String[parameters.length + 1];
        for (int index = 0; index < parameters.length; index++) {
            slots[index] = index < found.parameters().size()
                    ? found.parameters().get(index)
                    : UNKNOWN;
        }
        slots[parameters.length] = found.result();
        return slots;
    }

    private Map<String, MethodNullability> methodsOf(Class<?> declaring) {
        String binaryName = declaring.getName();
        Map<String, MethodNullability> cached = cache.get(binaryName);
        if (cached != null) {
            return cached;
        }
        Map<String, MethodNullability> parsed = parse(binaryName);
        cache.put(binaryName, parsed);
        return parsed;
    }

    private Map<String, MethodNullability> parse(String binaryName) {
        byte[] bytes = readBytes(binaryName);
        if (bytes == null) {
            return Map.of();
        }
        try {
            ClassModel model = ClassFile.of().parse(bytes);
            Map<String, MethodNullability> methods = new HashMap<>(model.methods().size() * 2);
            for (MethodModel method : model.methods()) {
                String key = method.methodName().stringValue() + method.methodType().stringValue();
                String result = UNKNOWN;
                List<String> parameters = List.of();
                for (MethodElement element : method.elementList()) {
                    if (element instanceof RuntimeInvisibleAnnotationsAttribute visible
                            && UNKNOWN.equals(result)) {
                        result = nullability(visible.annotations());
                    } else if (element instanceof RuntimeVisibleAnnotationsAttribute visible) {
                        // A runtime-visible jetbrains annotation is equivalent evidence.
                        String found = nullability(visible.annotations());
                        result = UNKNOWN.equals(found) ? result : found;
                    } else if (element instanceof RuntimeInvisibleParameterAnnotationsAttribute invisible) {
                        parameters = merge(parameters, invisible.parameterAnnotations());
                    } else if (element instanceof RuntimeVisibleParameterAnnotationsAttribute visible) {
                        parameters = merge(parameters, visible.parameterAnnotations());
                    }
                }
                methods.put(key, new MethodNullability(result, parameters));
            }
            return methods;
        } catch (RuntimeException | LinkageError malformed) {
            // Nullability is optional evidence: an unreadable class file is not a failed query.
            Facts.warn("cannot read class file for " + binaryName + ": " + malformed);
            return Map.of();
        }
    }

    /** Combined per-parameter answers; the first attribute that proves a slot wins. */
    private static List<String> merge(List<String> current,
            List<List<Annotation>> attributeParameters) {
        List<String> merged = new ArrayList<>(Math.max(current.size(), attributeParameters.size()));
        merged.addAll(current);
        for (int index = 0; index < attributeParameters.size(); index++) {
            String found = nullability(attributeParameters.get(index));
            if (index >= merged.size()) {
                merged.add(found);
            } else if (UNKNOWN.equals(merged.get(index)) && !UNKNOWN.equals(found)) {
                merged.set(index, found);
            }
        }
        return List.copyOf(merged);
    }

    /** {@code non_null}/{@code nullable} only when a jetbrains annotation says so. */
    private static String nullability(List<Annotation> annotations) {
        for (Annotation annotation : annotations) {
            String type = binaryName(annotation);
            if (NOT_NULL.equals(type)) {
                return "non_null";
            }
            if (NULLABLE.equals(type)) {
                return "nullable";
            }
        }
        return UNKNOWN;
    }

    /** {@code Lorg/jetbrains/annotations/NotNull;} to {@code org.jetbrains.annotations.NotNull}. */
    private static String binaryName(Annotation annotation) {
        String descriptor = annotation.classSymbol().descriptorString();
        if (descriptor.startsWith("L") && descriptor.endsWith(";")) {
            return descriptor.substring(1, descriptor.length() - 1).replace('/', '.');
        }
        return descriptor;
    }

    private byte[] readBytes(String binaryName) {
        String resource = binaryName.replace('.', '/') + ".class";
        try (InputStream stream = loader.getResourceAsStream(resource)) {
            if (stream == null) {
                return null;
            }
            return stream.readAllBytes();
        } catch (IOException | RuntimeException unreadable) {
            Facts.warn("cannot read " + resource + ": " + unreadable);
            return null;
        }
    }
}
