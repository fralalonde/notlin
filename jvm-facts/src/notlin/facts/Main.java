package notlin.facts;

import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.InputStreamReader;
import java.io.OutputStreamWriter;
import java.io.Writer;
import java.nio.charset.StandardCharsets;

/**
 * Stdio entry point of the notlin JVM facts helper.
 *
 * <p>Reads one request per line from stdin, writes exactly one response line per
 * answered request to stdout and flushes it. The wire format is fixed by our own
 * two producers (Rust {@code src/facts/protocol.rs}, this helper), so the parser
 * below is a deliberately rigid string scanner instead of a JSON library: no
 * third-party dependency may enter the helper. Anything that does not match the
 * version-1 shape is dropped without an answer and the loop keeps serving, so a
 * malformed line can never take the process down (the Rust side maps a missing
 * answer to {@code Unknown}).
 *
 * <p>Requests:
 * <pre>
 * {"version":1,"id":N,"query":{"Ping":null}}
 * {"version":1,"id":N,"query":{"ClassMembers":{"owner":"binary.Name"}}}
 * </pre>
 */
public final class Main {

    private Main() {
    }

    public static void main(String[] args) throws Exception {
        Facts facts = Facts.create();
        BufferedReader input = new BufferedReader(new InputStreamReader(System.in, StandardCharsets.UTF_8));
        Writer output = new BufferedWriter(new OutputStreamWriter(System.out, StandardCharsets.UTF_8));
        String line;
        while ((line = input.readLine()) != null) {
            String response;
            try {
                response = handle(line, facts);
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
    }

    /**
     * Answers one request line, or returns {@code null} when the line carries no
     * answerable query (malformed, unknown discriminator, unsupported version).
     */
    static String handle(String line, Facts facts) {
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
        switch (discriminator) {
            case "Ping" -> {
                cursor.literal("null");
            }
            case "ClassMembers" -> {
                cursor.expect('{');
                cursor.requireKey("owner");
                String owner = cursor.string();
                cursor.expect('}');
                // close "ClassMembers" is already closed by the inner object;
                // close the query object and the envelope.
                cursor.expect('}');
                cursor.expect('}');
                cursor.end();
                return facts.membersAnswer(id, owner);
            }
            default -> throw new IllegalArgumentException("unknown query discriminator \"" + discriminator + "\"");
        }
        cursor.expect('}');
        cursor.expect('}');
        cursor.end();
        return pong(id);
    }

    private static String pong(long id) {
        return "{\"version\":" + Facts.PROTOCOL_VERSION + ",\"id\":" + id
                + ",\"answer\":{\"Pong\":{\"version\":" + Facts.PROTOCOL_VERSION + "}}}";
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

        void literal(String literal) {
            skipWhitespace();
            if (!text.startsWith(literal, at)) {
                throw malformed("expected " + literal + " at offset " + at);
            }
            at += literal.length();
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
            // Envelope fields are comma-separated; the first one has no leading comma.
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
