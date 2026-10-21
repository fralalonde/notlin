package notlin.facts.kotlin;

import com.intellij.openapi.Disposable;
import com.intellij.openapi.util.Disposer;
import kotlin.Unit;
import kotlin.jvm.functions.Function1;
import org.jetbrains.kotlin.analysis.api.projectStructure.KaSourceModule;
import org.jetbrains.kotlin.analysis.api.standalone.StandaloneAnalysisAPISession;
import org.jetbrains.kotlin.analysis.api.standalone.StandaloneAnalysisAPISessionBuilder;
import org.jetbrains.kotlin.analysis.api.standalone.StandaloneAnalysisAPISessionBuilderKt;
import org.jetbrains.kotlin.analysis.project.structure.builder.KaModuleContainerBuilder;
import org.jetbrains.kotlin.analysis.project.structure.builder.KtLibraryModuleBuilder;
import org.jetbrains.kotlin.analysis.project.structure.builder.KtSourceModuleBuilder;
import org.jetbrains.kotlin.platform.jvm.JvmPlatforms;
import org.jetbrains.kotlin.psi.KtFile;

import java.io.IOException;
import java.net.URISyntaxException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.security.CodeSource;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.stream.Stream;

/**
 * A long-lived headless K2 {@code StandaloneAnalysisAPISession} over the Kotlin
 * sources of one translation run.
 *
 * <p>Built once at helper startup (measured ~1 s) and then fed one
 * {@code ResolveCalls} batch per file: the first batch on a fresh session pays
 * the FIR frontend warm-up (1.0-1.3 s on the spike), later batches are tens of
 * milliseconds per file. That amortization is the whole point of putting this in
 * the stdio helper instead of spawning a JVM per query.
 *
 * <h2>Environment contract</h2>
 * <ul>
 *   <li>{@code NOTLIN_KOTLIN_SOURCES} — <strong>required</strong>, colon-separated
 *       entries, each a directory or a {@code .kt} file. Directories are added as
 *       source roots (recursively indexed); a file entry adds its <em>parent
 *       directory</em> as the root, because the Analysis API indexes by root and
 *       cross-file resolution ({@code Counter.add} called from {@code Site.kt})
 *       needs the siblings in the same module.</li>
 *   <li>{@code NOTLIN_CLASSPATH} — optional, colon-separated jars/class
 *       directories, same variable the Track A {@code Facts} loader uses. They
 *       are added as one extra library module. Best effort: an unresolvable
 *       external type becomes an error type and the call site degrades to
 *       {@code Ambiguous}, never to a guess.</li>
 *   <li>{@code NOTLIN_KOTLIN_STDLIB} — optional explicit stdlib jar; by default
 *       the jar is located from the loaded {@code kotlin.Unit} class's code
 *       source (the full jar puts the pinned deps on its manifest
 *       {@code Class-Path}, so that is the stdlib from {@code deps.lock}).</li>
 * </ul>
 *
 * <p>Session files are addressed by real path ({@link Path#toRealPath()}), so a
 * query for a path that is not in the indexed module answers nothing at all:
 * the caller maps a missing answer to {@code Unknown} rather than accepting an
 * empty site list for a file we never read.
 *
 * <p>Sharp edges from {@code docs/analysis-api-spike.md} honored here: every
 * module is registered with {@code addModule}, {@code platform} is set on each
 * module (not only on the container), and {@link #shutdown()} terminates the JVM
 * explicitly — the standalone session leaves non-daemon threads behind, so a
 * plain return from {@code main} would hang the process forever.
 */
public final class KotlinSession {

    private final Disposable disposable;
    private final StandaloneAnalysisAPISession session;
    private final KaSourceModule module;
    /** Real path of every indexed {@code .kt} file -> its PSI file. */
    private final Map<String, KtFile> files;
    private final long startupMs;

    private KotlinSession(Disposable disposable, StandaloneAnalysisAPISession session,
            KaSourceModule module, Map<String, KtFile> files, long startupMs) {
        this.disposable = disposable;
        this.session = session;
        this.module = module;
        this.files = files;
        this.startupMs = startupMs;
    }

    public long startupMs() {
        return startupMs;
    }

    public int fileCount() {
        return files.size();
    }

    /**
     * Builds the session from the environment.
     *
     * @throws IllegalStateException when {@code NOTLIN_KOTLIN_SOURCES} is unset or
     *         empty, names no {@code .kt} file, or the stdlib jar cannot be found —
     *         the caller treats that as a startup failure (exit 2) instead of
     *         serving queries it can never answer.
     */
    public static KotlinSession open() {
        List<Path> roots = new ArrayList<>();
        Set<Path> requestedFiles = new LinkedHashSet<>();
        for (String entry : splitPathList(System.getenv("NOTLIN_KOTLIN_SOURCES"))) {
            Path path = Paths.get(entry).toAbsolutePath().normalize();
            if (Files.isDirectory(path)) {
                roots.add(path);
            } else if (Files.isRegularFile(path)) {
                requestedFiles.add(path);
                Path parent = path.getParent();
                if (parent == null) {
                    throw new IllegalStateException("source file has no parent directory: " + path);
                }
                roots.add(parent);
            } else {
                warn("ignoring missing NOTLIN_KOTLIN_SOURCES entry: " + path);
            }
        }
        if (roots.isEmpty()) {
            throw new IllegalStateException(
                    "NOTLIN_KOTLIN_SOURCES names no existing directory or .kt file (got: "
                            + System.getenv("NOTLIN_KOTLIN_SOURCES") + ")");
        }
        // Distinct, stable order: LinkedHashSet above already deduplicated only
        // files; roots may repeat when several files share a directory.
        List<Path> distinctRoots = new ArrayList<>(new LinkedHashSet<>(roots));
        Path stdlib = locateStdlib();

        long started = System.nanoTime();
        Disposable disposable = Disposer.newDisposable();
        KaSourceModule[] moduleRef = new KaSourceModule[1];
        StandaloneAnalysisAPISession session;
        try {
            session = StandaloneAnalysisAPISessionBuilderKt.buildStandaloneAnalysisAPISession(
                    disposable, false,
                    (Function1<StandaloneAnalysisAPISessionBuilder, Unit>) builder -> {
                        builder.buildKtModuleProvider(
                                (Function1<KaModuleContainerBuilder, Unit>) container -> {
                                    container.setPlatform(JvmPlatforms.INSTANCE.getDefaultJvmPlatform());
                                    var dependencies = new ArrayList<org.jetbrains.kotlin.analysis.api.projectStructure.KaModule>();
                                    dependencies.add(buildJdkModule(container));
                                    dependencies.add(buildLibraryModule(container, "kotlin-stdlib", List.of(stdlib)));
                                    List<Path> classpath = existingClasspath();
                                    if (!classpath.isEmpty()) {
                                        dependencies.add(buildLibraryModule(container, "notlin-classpath", classpath));
                                    }
                                    KaSourceModule sourceModule = buildSourceModule(container, distinctRoots, dependencies);
                                    container.addModule(sourceModule);
                                    moduleRef[0] = sourceModule;
                                    return Unit.INSTANCE;
                                });
                        return Unit.INSTANCE;
                    });
        } catch (RuntimeException | LinkageError failure) {
            Disposer.dispose(disposable);
            throw new IllegalStateException("Analysis API session build failed: " + failure, failure);
        }
        long builtMs = (System.nanoTime() - started) / 1_000_000L;

        Map<String, KtFile> indexed = new LinkedHashMap<>();
        for (KtFile file : sessionFiles(session, moduleRef[0])) {
            String path = realPathOf(file);
            if (path != null) {
                indexed.put(path, file);
            }
        }
        if (indexed.isEmpty()) {
            // Sharp edge 1 (spike): a module that is not registered still builds,
            // it just carries no files. Never serve from an empty index silently.
            Disposer.dispose(disposable);
            throw new IllegalStateException(
                    "Analysis API session indexed no Kotlin files for roots " + distinctRoots);
        }
        for (Path requested : requestedFiles) {
            String real = realPath(requested.toString());
            if (real == null || !indexed.containsKey(real)) {
                warn("requested source file is not in the indexed module: " + requested);
            }
        }
        KotlinSession built = new KotlinSession(disposable, session, moduleRef[0], indexed, builtMs);
        log("session ready in " + builtMs + " ms: files=" + indexed.size()
                + " roots=" + distinctRoots + " stdlib=" + stdlib.getFileName()
                + " classpath=" + existingClasspath().size() + " entries");
        return built;
    }

    /** Answers one {@code ResolveCalls} batch, or {@code null} when the file is not indexed. */
    public String callSitesAnswer(long id, String requestedPath) {
        String real = realPath(requestedPath);
        KtFile file = real == null ? null : files.get(real);
        if (file == null) {
            warn("ResolveCalls: file is not indexed by this session: " + requestedPath);
            return null;
        }
        byte[] raw;
        try {
            raw = Files.readAllBytes(Paths.get(real));
        } catch (IOException unreadable) {
            warn("ResolveCalls: cannot read " + real + ": " + unreadable);
            return null;
        }
        long started = System.nanoTime();
        List<CallSites.Site> sites;
        try {
            sites = CallSites.extractAll(session, file, raw);
        } catch (RuntimeException | LinkageError failure) {
            // Fail closed: an exception mid-file must not produce a partial batch
            // the Rust side could mistake for "every call site in this file".
            warn("ResolveCalls failed for " + real + ": " + failure);
            return null;
        }
        long elapsed = (System.nanoTime() - started) / 1_000_000L;
        log("ResolveCalls " + Paths.get(real).getFileName() + " sites=" + sites.size() + " in " + elapsed + " ms");
        return CallSites.toAnswerLine(id, requestedPath, sites);
    }

    /**
     * Disposes the session and terminates the JVM.
     *
     * <p>The standalone session keeps non-daemon threads alive (spike sharp edge
     * 3), so returning from {@code main} would leave the helper running forever.
     * Disposal is given a bounded window on a daemon thread and the JVM is then
     * halted, which keeps EOF-to-exit well under the 2 s budget the protocol
     * requires.
     */
    public void shutdown() {
        Thread dispose = new Thread(() -> {
            try {
                Disposer.dispose(disposable);
            } catch (RuntimeException | LinkageError failure) {
                warn("session disposal failed: " + failure);
            }
        }, "notlin-kotlin-dispose");
        dispose.setDaemon(true);
        dispose.start();
        try {
            dispose.join(1500L);
        } catch (InterruptedException interrupted) {
            Thread.currentThread().interrupt();
        }
        Runtime.getRuntime().halt(0);
    }

    /**
     * The JDK itself, as an sdk module.
     *
     * <p>This is not optional: without it the session cannot see {@code java.lang}
     * at all, so any source that touches a Java type produces
     * {@code MISSING_DEPENDENCY_SUPERCLASS}/{@code UNRESOLVED_REFERENCE} and every
     * call site near it collapses to {@code Ambiguous} (found on the bench slice:
     * 168 errors in one file, all traceable to an invisible JDK).
     */
    private static org.jetbrains.kotlin.analysis.api.projectStructure.KaModule buildJdkModule(
            KaModuleContainerBuilder container) {
        var jdk = org.jetbrains.kotlin.analysis.project.structure.builder.KaSdkModuleBuilderKt
                .buildKtSdkModule(container,
                        (Function1<org.jetbrains.kotlin.analysis.project.structure.builder.KtSdkModuleBuilder, Unit>) builder -> {
                            builder.setLibraryName("jdk");
                            builder.setPlatform(JvmPlatforms.INSTANCE.getDefaultJvmPlatform());
                            builder.addBinaryRootsFromJdkHome(
                                    Paths.get(System.getProperty("java.home")), true);
                            return Unit.INSTANCE;
                        });
        container.addModule(jdk);
        return jdk;
    }

    private static org.jetbrains.kotlin.analysis.api.projectStructure.KaModule buildLibraryModule(
            KaModuleContainerBuilder container, String name, List<Path> roots) {
        var library = org.jetbrains.kotlin.analysis.project.structure.builder.KaLibraryModuleBuilderKt
                .buildKtLibraryModule(container, (Function1<KtLibraryModuleBuilder, Unit>) builder -> {
                    builder.setLibraryName(name);
                    // Sharp edge 2: the platform must be set on every module, not
                    // only on the container, or build() throws
                    // UninitializedPropertyAccessException.
                    builder.setPlatform(JvmPlatforms.INSTANCE.getDefaultJvmPlatform());
                    for (Path root : roots) {
                        builder.addBinaryRoot(root);
                    }
                    return Unit.INSTANCE;
                });
        container.addModule(library);
        return library;
    }

    private static KaSourceModule buildSourceModule(KaModuleContainerBuilder container, List<Path> roots,
            List<org.jetbrains.kotlin.analysis.api.projectStructure.KaModule> dependencies) {
        return org.jetbrains.kotlin.analysis.project.structure.builder.KaSourceModuleBuilderKt
                .buildKtSourceModule(container, (Function1<KtSourceModuleBuilder, Unit>) builder -> {
                    builder.setModuleName("notlin-sources");
                    builder.setPlatform(JvmPlatforms.INSTANCE.getDefaultJvmPlatform());
                    builder.addSourceRoots(roots);
                    for (var dependency : dependencies) {
                        builder.addRegularDependency(dependency);
                    }
                    return Unit.INSTANCE;
                });
    }

    private static List<KtFile> sessionFiles(StandaloneAnalysisAPISession session, KaSourceModule module) {
        var byModule = session.getModulesWithFiles();
        var found = byModule.get(module);
        return found == null ? List.of() : found.stream().filter(KtFile.class::isInstance)
                .map(KtFile.class::cast).toList();
    }

    /** {@code NOTLIN_CLASSPATH} entries that exist, minus anything we cannot read. */
    private static List<Path> existingClasspath() {
        List<Path> entries = new ArrayList<>();
        for (String raw : splitPathList(System.getenv("NOTLIN_CLASSPATH"))) {
            Path path = Paths.get(raw).toAbsolutePath().normalize();
            if (Files.exists(path)) {
                entries.add(path);
            } else {
                warn("ignoring missing NOTLIN_CLASSPATH entry: " + path);
            }
        }
        return entries;
    }

    /**
     * Finds the kotlin-stdlib jar the session should resolve against, in order:
     * {@code NOTLIN_KOTLIN_STDLIB}, the code source of the already-loaded
     * {@code kotlin.Unit} class (the full jar's manifest {@code Class-Path} puts
     * the deps-locked stdlib there), {@code java.class.path}, then the libs
     * directory next to the helper jar.
     */
    private static Path locateStdlib() {
        String configured = System.getenv("NOTLIN_KOTLIN_STDLIB");
        if (configured != null && !configured.isBlank()) {
            Path path = Paths.get(configured.trim()).toAbsolutePath().normalize();
            if (!Files.isRegularFile(path)) {
                throw new IllegalStateException("NOTLIN_KOTLIN_STDLIB is not a file: " + path);
            }
            return path;
        }
        try {
            CodeSource source = kotlin.Unit.class.getProtectionDomain().getCodeSource();
            if (source != null && source.getLocation() != null) {
                Path path = Paths.get(source.getLocation().toURI());
                if (Files.isRegularFile(path) && path.getFileName().toString().startsWith("kotlin-stdlib")) {
                    return path;
                }
            }
        } catch (URISyntaxException | RuntimeException ignored) {
            // fall through to the cheaper guesses below
        }
        for (String entry : splitPathList(System.getProperty("java.class.path"))) {
            Path path = Paths.get(entry);
            if (Files.isRegularFile(path) && path.getFileName().toString().startsWith("kotlin-stdlib")) {
                return path.toAbsolutePath().normalize();
            }
        }
        for (Path candidate : libsDirectories()) {
            Path found = firstStdlibJar(candidate);
            if (found != null) {
                return found;
            }
        }
        throw new IllegalStateException(
                "kotlin-stdlib jar not found; set NOTLIN_KOTLIN_STDLIB or run jvm-facts/fetch-deps.sh");
    }

    private static List<Path> libsDirectories() {
        List<Path> directories = new ArrayList<>();
        directories.add(Paths.get("spike/libs"));
        directories.add(Paths.get("jvm-facts/spike/libs"));
        try {
            CodeSource self = KotlinSession.class.getProtectionDomain().getCodeSource();
            if (self != null && self.getLocation() != null) {
                Path location = Paths.get(self.getLocation().toURI());
                Path base = Files.isDirectory(location) ? location : location.getParent();
                if (base != null) {
                    directories.add(base.resolveSibling("libs"));
                    directories.add(base.getParent() == null ? base : base.getParent().resolve("spike/libs"));
                }
            }
        } catch (URISyntaxException | RuntimeException ignored) {
            // best effort only
        }
        return directories;
    }

    private static Path firstStdlibJar(Path directory) {
        if (!Files.isDirectory(directory)) {
            return null;
        }
        try (Stream<Path> listing = Files.list(directory)) {
            return listing.filter(path -> {
                String name = path.getFileName().toString();
                return name.startsWith("kotlin-stdlib") && name.endsWith(".jar");
            }).sorted().findFirst().map(path -> path.toAbsolutePath().normalize()).orElse(null);
        } catch (IOException unreadable) {
            return null;
        }
    }

    private static String realPathOf(KtFile file) {
        if (file.getVirtualFile() == null || !file.getVirtualFile().isValid()) {
            return null;
        }
        return realPath(file.getVirtualFile().getPath());
    }

    /** Canonical form used for both the index and every query, so symlinks match. */
    private static String realPath(String path) {
        try {
            return Paths.get(path).toRealPath().toString();
        } catch (IOException missing) {
            return null;
        }
    }

    private static List<String> splitPathList(String value) {
        if (value == null) {
            return List.of();
        }
        List<String> entries = new ArrayList<>();
        for (String entry : value.split(":")) {
            String trimmed = entry.trim();
            if (!trimmed.isEmpty()) {
                entries.add(trimmed);
            }
        }
        return entries;
    }

    private static void log(String message) {
        System.err.println("jvm-facts: " + message);
    }

    private static void warn(String message) {
        log(message);
    }

    /** Exposed for diagnostics only: the module the session was built around. */
    KaSourceModule module() {
        return module;
    }

    /** Exposed for diagnostics only. */
    StandaloneAnalysisAPISession session() {
        return session;
    }
}
