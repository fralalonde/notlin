package notlin.facts;

import notlin.facts.kotlin.KotlinSession;

import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.InputStreamReader;
import java.io.OutputStreamWriter;
import java.io.Writer;
import java.nio.charset.StandardCharsets;

/**
 * Entry point of the <em>full</em> helper jar — the Track A stdio loop plus the
 * Track B Kotlin facts behind {@code --kotlin}.
 *
 * <pre>
 * java -jar notlin-jvm-facts-full.jar              # identical to the slim jar
 * java -jar notlin-jvm-facts-full.jar --kotlin     # + ResolveCalls
 * </pre>
 *
 * <p>The slim jar keeps {@link Main} as its main class and is built from exactly
 * the sources it always was; this class and everything under
 * {@code notlin.facts.kotlin} only exist in the full jar. Dispatch is delegated
 * to {@link Main#handle} for every request the slim helper already understood, so
 * the two jars cannot drift on {@code Ping}/{@code ClassMembers} — the only new
 * behavior is the {@code ResolveCalls} discriminator.
 *
 * <p><strong>Fail closed.</strong> Without {@code --kotlin} a {@code ResolveCalls}
 * request produces <em>no answer line</em> (exactly what the pre-Kotlin helper
 * does with an unknown discriminator), and the loop keeps serving. With
 * {@code --kotlin}, a request for a file the session did not index, or a batch
 * that fails midway, also produces no answer line rather than a partial one: the
 * Rust side maps a missing answer to {@code Unknown}, which is the truth.
 *
 * <p>On stdin EOF the session is disposed and the JVM is halted — the standalone
 * K2 session leaks non-daemon threads, so a plain return from {@code main} would
 * keep the helper alive forever (spike sharp edge 3).
 */
public final class FullMain {

    private FullMain() {
    }

    public static void main(String[] args) throws Exception {
        boolean kotlin = false;
        for (String arg : args) {
            switch (arg) {
                case "--kotlin" -> kotlin = true;
                case "-h", "--help" -> {
                    System.err.println("usage: java -jar notlin-jvm-facts-full.jar [--kotlin]");
                    System.err.println("  --kotlin   also answer {\"ResolveCalls\":{\"file\":...}};");
                    System.err.println("             needs NOTLIN_KOTLIN_SOURCES (colon-separated .kt files or dirs)");
                    return;
                }
                default -> {
                    Facts.warn("ignoring unknown argument: " + arg);
                }
            }
        }

        Facts facts = Facts.create();
        KotlinSession kotlinSession = null;
        if (kotlin) {
            try {
                kotlinSession = KotlinSession.open();
            } catch (RuntimeException | LinkageError startupFailure) {
                // Serving without a session would answer nothing anyway; fail fast
                // and loudly (exit 2) so the parent is not left waiting on a helper
                // that can only ever answer Ping.
                Facts.warn("--kotlin unavailable: " + startupFailure.getMessage());
                System.exit(2);
            }
        }

        BufferedReader input = new BufferedReader(new InputStreamReader(System.in, StandardCharsets.UTF_8));
        Writer output = new BufferedWriter(new OutputStreamWriter(System.out, StandardCharsets.UTF_8));
        String line;
        while ((line = input.readLine()) != null) {
            String response;
            try {
                response = handle(line, facts, kotlinSession);
            } catch (RuntimeException malformed) {
                Facts.warn("ignoring unusable request: " + malformed.getMessage());
                continue;
            }
            if (response == null) {
                continue;
            }
            output.write(response);
            output.write('\n');
            output.flush();
        }
        output.flush();
        if (kotlinSession != null) {
            Facts.warn("stdin closed; disposing the Kotlin session");
            // shutdown() halts the JVM: the standalone session's non-daemon
            // threads would otherwise keep this process alive forever.
            kotlinSession.shutdown();
        }
    }

    /**
     * Answers one request line, or {@code null} when it carries no answerable
     * query. Only the discriminator is parsed here; everything the slim helper
     * understands is handed to {@link Main#handle} verbatim.
     */
    static String handle(String line, Facts facts, KotlinSession kotlinSession) {
        Cursor cursor = new Cursor(line);
        cursor.expect('{');
        cursor.requireKey("version");
        long version = cursor.number();
        if (version != Facts.PROTOCOL_VERSION) {
            throw new IllegalArgumentException("unsupported protocol version " + version);
        }
        cursor.requireKey("id");
        long id = cursor.number();
        cursor.requireKey("query");
        cursor.expect('{');
        String discriminator = cursor.key();
        if (!"ResolveCalls".equals(discriminator)) {
            return Main.handle(line, facts);
        }
        if (kotlinSession == null) {
            // No --kotlin: same as any other unknown discriminator, no answer.
            Facts.warn("ResolveCalls requires --kotlin; dropping request " + id);
            return null;
        }
        cursor.expect('{');
        cursor.requireKey("file");
        String file = cursor.string();
        cursor.expect('}');
        cursor.expect('}');
        cursor.expect('}');
        cursor.end();
        return kotlinSession.callSitesAnswer(id, file);
    }

    /** Minimal reader for the exact version-1 envelope; throws on any deviation. */
    private static final class Cursor {
        private final String text;
        private int at;

        Cursor(String text) {
            this.text = text;
        }

        void expect(char expected) {
            skipWhitespace();
            if (at >= text.length() || text.charAt(at) != expected) {
                throw malformed("expected '" + expected + "' at offset " + at);
            }
            at++;
        }

        long number() {
            skipWhitespace();
            int start = at;
            while (at < text.length() && Character.isDigit(text.charAt(at))) {
                at++;
            }
            if (start == at) {
                throw malformed("expected a number at offset " + start);
            }
            try {
                return Long.parseLong(text, start, at, 10);
            } catch (NumberFormatException outOfRange) {
                throw malformed("number out of range at offset " + start);
            }
        }

        String key() {
            String name = string();
            expect(':');
            return name;
        }

        void requireKey(String expected) {
            skipWhitespace();
            if (at < text.length() && text.charAt(at) == ',') {
                at++;
            }
            String found = key();
            if (!expected.equals(found)) {
                throw malformed("expected key \"" + expected + "\" but found \"" + found + "\"");
            }
        }

        String string() {
            skipWhitespace();
            if (at >= text.length() || text.charAt(at) != '"') {
                throw malformed("expected a string at offset " + at);
            }
            at++;
            StringBuilder value = new StringBuilder();
            while (true) {
                if (at >= text.length()) {
                    throw malformed("unterminated string");
                }
                char current = text.charAt(at++);
                if (current == '"') {
                    return value.toString();
                }
                if (current != '\\') {
                    value.append(current);
                    continue;
                }
                if (at >= text.length()) {
                    throw malformed("unterminated escape");
                }
                char escaped = text.charAt(at++);
                switch (escaped) {
                    case '"', '\\', '/' -> value.append(escaped);
                    case 'n' -> value.append('\n');
                    case 'r' -> value.append('\r');
                    case 't' -> value.append('\t');
                    case 'b' -> value.append('\b');
                    case 'f' -> value.append('\f');
                    case 'u' -> {
                        if (at + 4 > text.length()) {
                            throw malformed("truncated unicode escape");
                        }
                        value.append((char) Integer.parseInt(text.substring(at, at + 4), 16));
                        at += 4;
                    }
                    default -> throw malformed("unknown escape \\" + escaped);
                }
            }
        }

        void end() {
            skipWhitespace();
            if (at != text.length()) {
                throw malformed("trailing input at offset " + at);
            }
        }

        private void skipWhitespace() {
            while (at < text.length() && Character.isWhitespace(text.charAt(at))) {
                at++;
            }
        }

        private IllegalArgumentException malformed(String message) {
            return new IllegalArgumentException(message);
        }
    }
}
