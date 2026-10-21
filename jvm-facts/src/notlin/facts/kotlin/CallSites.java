package notlin.facts.kotlin;

import org.jetbrains.kotlin.analysis.api.KaSession;
import org.jetbrains.kotlin.analysis.api.AnalyzeKt;
import org.jetbrains.kotlin.analysis.api.resolution.KaCallInfoKt;
import org.jetbrains.kotlin.analysis.api.resolution.KaFunctionCall;
import org.jetbrains.kotlin.analysis.api.signatures.KaFunctionSignature;
import org.jetbrains.kotlin.analysis.api.signatures.KaVariableSignature;
import org.jetbrains.kotlin.analysis.api.symbols.KaConstructorSymbol;
import org.jetbrains.kotlin.analysis.api.symbols.KaSymbol;
import org.jetbrains.kotlin.analysis.api.symbols.KaValueParameterSymbol;
import org.jetbrains.kotlin.analysis.api.types.KaErrorType;
import org.jetbrains.kotlin.analysis.api.types.KaFlexibleType;
import org.jetbrains.kotlin.analysis.api.types.KaType;
import org.jetbrains.kotlin.analysis.api.standalone.StandaloneAnalysisAPISession;
import org.jetbrains.kotlin.name.CallableId;
import org.jetbrains.kotlin.psi.KtCallExpression;
import org.jetbrains.kotlin.psi.KtFile;
import org.jetbrains.kotlin.types.Variance;
import com.intellij.psi.util.PsiTreeUtil;

import kotlin.jvm.functions.Function1;

import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;

/**
 * One {@code ResolveCalls} batch: every call expression in a file, resolved in a
 * single {@code analyze(file) { }} block (one FIR acquire per file instead of one
 * per site).
 *
 * <h2>Status mapping (fail closed)</h2>
 * <ul>
 *   <li>{@code Established} — the call resolved to exactly one callable and every
 *       type on the path (expression, parameters, result) is a real K2 type. All
 *       fact fields are filled.</li>
 *   <li>{@code Ambiguous} — resolution succeeded but touched an <em>error
 *       type</em>, or resolution failed while K2 did see candidates. Nothing about
 *       the callee is emitted: an error type means the analysis is incomplete, and
 *       filling the rest in would be a guess.</li>
 *   <li>{@code Unknown} — resolution failed with no candidates at all (an
 *       unresolved name, or a construct K2 does not model as a function call).</li>
 * </ul>
 * There is no path from "not proven" to a populated callee: {@code Established} is
 * the only status whose fields are meaningful.
 *
 * <h2>Field semantics</h2>
 * <ul>
 *   <li>{@code callee_owner} — the K2 {@code ClassId} ({@code fixture.Counter},
 *       {@code kotlin.collections.MutableList}), <em>not</em> the JVM binary name.
 *       It is {@code null} for top-level functions, whose JVM owner would be a
 *       file facade ({@code FooKt}), and for constructors it is the constructed
 *       class.</li>
 *   <li>{@code callee_name} — Kotlin simple name; {@code <init>} for constructors.</li>
 *   <li>{@code parameter_types} / {@code result_type} — K2 renderings with
 *       {@link Variance#INVARIANT} ({@code kotlin.Int}, {@code kotlin.String?},
 *       {@code kotlin.collections.MutableList<kotlin.Int>}), already substituted at
 *       the call site.</li>
 *   <li>{@code result_nullable} — true when the result type is marked nullable or
 *       has flexible (Java platform) nullability. Descriptors are erased, so this
 *       is the only nullability signal on the wire.</li>
 *   <li>{@code jvm_descriptor} — erased method descriptor
 *       ({@code (I)Ljava/lang/String;}), {@code (…)V} for constructors.</li>
 *   <li>{@code byte_offset} — UTF-8 byte offset of the call expression in the file
 *       as it exists on disk (PSI offsets are char offsets in a line-ending
 *       normalized copy, so they are mapped, not copied), which is what the Rust
 *       lexer's offsets refer to.</li>
 * </ul>
 */
final class CallSites {

    private CallSites() {
    }

    /** One resolved call site, already flattened to protocol values. */
    record Site(
            int byteOffset,
            String text,
            String status,
            String calleeOwner,
            String calleeName,
            List<String> parameterTypes,
            String resultType,
            boolean resultNullable,
            String jvmDescriptor) {
    }

    static final String ESTABLISHED = "Established";
    static final String AMBIGUOUS = "Ambiguous";
    static final String UNKNOWN = "Unknown";

    /**
     * Resolves every call expression in {@code file} inside one analysis session.
     *
     * @throws RuntimeException when the session or a type provider fails outright;
     *         the caller drops the whole batch instead of answering partially.
     */
    @SuppressWarnings("deprecation") // analyze(KtElement, ...) is what the spike validated; the
                                     // non-deprecated overload takes a KaModule, not a file.
    static List<Site> extractAll(StandaloneAnalysisAPISession session, KtFile file, byte[] raw) {
        List<KtCallExpression> calls = new ArrayList<>(PsiTreeUtil.findChildrenOfType(file, KtCallExpression.class));
        String psiText = file.getText();
        List<Site> sites = new ArrayList<>(calls.size());
        if (calls.isEmpty()) {
            return sites;
        }
        AnalyzeKt.analyze(file, (Function1<KaSession, List<Site>>) analysis -> {
            for (KtCallExpression call : calls) {
                sites.add(site(analysis, call, raw, psiText));
            }
            return sites;
        });
        return sites;
    }

    private static Site site(KaSession analysis, KtCallExpression call, byte[] raw, String psiText) {
        int byteOffset = Offsets.byteOffset(raw, psiText, call.getTextOffset());
        String text = call.getText();
        KaFunctionCall<?> resolved;
        int candidateCount;
        try {
            resolved = KaCallInfoKt.successfulFunctionCallOrNull(analysis.resolveToCall(call));
        } catch (RuntimeException | LinkageError resolutionFailure) {
            return degraded(byteOffset, text, UNKNOWN);
        }
        if (resolved == null) {
            try {
                candidateCount = analysis.resolveToCallCandidates(call).size();
            } catch (RuntimeException | LinkageError candidateFailure) {
                candidateCount = 0;
            }
            // K2 saw candidates but could not pick one -> ambiguous, not unknown.
            return degraded(byteOffset, text, candidateCount > 0 ? AMBIGUOUS : UNKNOWN);
        }
        try {
            return established(analysis, call, resolved, byteOffset, text);
        } catch (RuntimeException | LinkageError extractionFailure) {
            // A type provider blew up mid-site: the resolution itself was fine but
            // the facts are incomplete, so this site is ambiguous, never guessed.
            return degraded(byteOffset, text, AMBIGUOUS);
        }
    }

    private static Site established(KaSession analysis, KtCallExpression call, KaFunctionCall<?> resolved,
            int byteOffset, String text) {
        KaFunctionSignature<?> signature = resolved.getSignature();
        KaSymbol symbol = signature.getSymbol();

        KaType expressionType = analysis.getExpressionType(call);
        KaType returnType = signature.getReturnType();
        if (isError(expressionType) || isError(returnType)) {
            return degraded(byteOffset, text, AMBIGUOUS);
        }
        List<String> parameterTypes = new ArrayList<>();
        List<String> parameterDescriptors = new ArrayList<>();
        for (KaVariableSignature<KaValueParameterSymbol> parameter : signature.getValueParameters()) {
            KaType parameterType = parameter.getReturnType();
            if (isError(parameterType)) {
                return degraded(byteOffset, text, AMBIGUOUS);
            }
            parameterTypes.add(render(analysis, parameterType));
            parameterDescriptors.add(analysis.mapToJvmTypeDescriptor(parameterType));
        }

        String owner;
        String name;
        String descriptor;
        if (symbol instanceof KaConstructorSymbol constructor) {
            // Sharp edge 5: constructors report callableId == null; the class id
            // comes from the constructor symbol itself.
            owner = constructor.getContainingClassId() == null
                    ? null
                    : constructor.getContainingClassId().asFqNameString();
            name = "<init>";
            descriptor = "(" + String.join("", parameterDescriptors) + ")V";
        } else {
            CallableId callableId = signature.getCallableId();
            owner = callableId == null || callableId.getClassId() == null
                    ? jvmOwnerOf(analysis, symbol)
                    : callableId.getClassId().asFqNameString();
            // Local functions have no callableId; the PSI text is the honest
            // fallback name (and the owner stays null: nothing JVM-addressable
            // is known about it).
            name = callableId != null
                    ? callableId.getCallableName().asString()
                    : calleeText(call);
            descriptor = "(" + String.join("", parameterDescriptors) + ")"
                    + analysis.mapToJvmTypeDescriptor(returnType);
        }
        if (name == null) {
            // A call we cannot name is not an established fact, whatever else we
            // know about it.
            return degraded(byteOffset, text, AMBIGUOUS);
        }
        return new Site(byteOffset, text, ESTABLISHED, owner, name, List.copyOf(parameterTypes),
                render(analysis, returnType), isNullable(analysis, returnType), descriptor);
    }

    /** Everything unknown: the shape stays fixed, the status carries the meaning. */
    private static Site degraded(int byteOffset, String text, String status) {
        return new Site(byteOffset, text, status, null, null, List.of(), null, false, null);
    }

    private static boolean isError(KaType type) {
        return type instanceof KaErrorType;
    }

    /**
     * Kotlin rendering of a type, normalized so the wire carries type <em>names</em>
     * rather than K2's flexibility notation:
     * <ul>
     *   <li>a flexible (Java platform) top-level type renders as its lower bound,
     *       because K2's own {@code A..B} form is not a Kotlin type name;</li>
     *   <li>K2's trailing {@code !} markers (flexible types, including inside type
     *       arguments) are dropped: {@code kotlin.String!} -> {@code kotlin.String},
     *       {@code (kotlin.String!) -> kotlin.Boolean} ->
     *       {@code (kotlin.String) -> kotlin.Boolean}.</li>
     * </ul>
     * Nullability is therefore <em>not</em> inferable from a rendered name — which
     * is exactly why the protocol has a separate {@code result_nullable}. The wire
     * has no per-parameter nullability field, so a dropped {@code !} on a parameter
     * is information the consumer never had a slot for.
     */
    private static String render(KaSession analysis, KaType type) {
        KaType shown = type instanceof KaFlexibleType flexible ? flexible.getLowerBound() : type;
        String text = org.jetbrains.kotlin.analysis.api.renderer.RenderingKt
                .render(analysis, shown, Variance.INVARIANT);
        return text.replace("!", "");
    }

    /**
     * Nullable or flexibly-nullable, i.e. "can this be null at runtime". Java
     * platform types are flexible, so they answer true: a translator that emits
     * the null check stays correct either way.
     */
    private static boolean isNullable(KaSession analysis, KaType type) {
        return analysis.isMarkedNullable(type) || analysis.getHasFlexibleNullability(type);
    }

    /**
     * JVM owner of a callable that has no {@code ClassId} — top-level functions
     * live on a file facade ({@code …Kt}), which is exactly the class the
     * translator has to call. {@code getContainingJvmClassName} answers it from
     * the same analysis; when it cannot, the owner stays {@code null} rather than
     * being invented.
     */
    private static String jvmOwnerOf(KaSession analysis, KaSymbol symbol) {
        if (!(symbol instanceof org.jetbrains.kotlin.analysis.api.symbols.KaCallableSymbol callable)) {
            return null;
        }
        try {
            String internal = analysis.getContainingJvmClassName(callable);
            return internal == null || internal.isEmpty() ? null : internal.replace('/', '.');
        } catch (RuntimeException | LinkageError unresolved) {
            return null;
        }
    }

    private static String calleeText(KtCallExpression call) {
        return call.getCalleeExpression() == null ? null : call.getCalleeExpression().getText();
    }

    /** The pinned {@code CallSites} answer line for one request id. */
    static String toAnswerLine(long id, String file, List<Site> sites) {
        StringBuilder line = new StringBuilder(64 + sites.size() * 192);
        line.append("{\"version\":1,\"id\":").append(id).append(",\"answer\":{\"CallSites\":{\"file\":");
        appendString(line, file);
        line.append(",\"sites\":[");
        for (int index = 0; index < sites.size(); index++) {
            if (index > 0) {
                line.append(',');
            }
            appendSite(line, sites.get(index));
        }
        line.append("]}}}");
        return line.toString();
    }

    private static void appendSite(StringBuilder out, Site site) {
        out.append("{\"byte_offset\":").append(site.byteOffset());
        out.append(",\"text\":");
        appendString(out, site.text());
        out.append(",\"status\":");
        appendString(out, site.status());
        out.append(",\"callee_owner\":");
        appendNullableString(out, site.calleeOwner());
        out.append(",\"callee_name\":");
        appendNullableString(out, site.calleeName());
        out.append(",\"parameter_types\":[");
        List<String> parameters = site.parameterTypes();
        for (int index = 0; index < parameters.size(); index++) {
            if (index > 0) {
                out.append(',');
            }
            appendString(out, parameters.get(index));
        }
        out.append("],\"result_type\":");
        appendNullableString(out, site.resultType());
        out.append(",\"result_nullable\":").append(site.resultNullable());
        out.append(",\"jvm_descriptor\":");
        appendNullableString(out, site.jvmDescriptor());
        out.append('}');
    }

    private static void appendNullableString(StringBuilder out, String value) {
        if (value == null) {
            out.append("null");
        } else {
            appendString(out, value);
        }
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

    /** PSI char offset -> UTF-8 byte offset in the on-disk file. */
    static final class Offsets {

        private Offsets() {
        }

        /**
         * The PSI text is a <em>normalized</em> view of the file (IntelliJ rewrites
         * CRLF and a leading BOM), while the Rust lexer's offsets are byte offsets
         * into the bytes on disk. When the two texts agree the mapping is a plain
         * UTF-8 prefix length; otherwise the file is walked code point by code
         * point, counting the bytes that are really there and the characters PSI
         * would have produced.
         */
        static int byteOffset(byte[] raw, String psiText, int charOffset) {
            String rawText = new String(raw, StandardCharsets.UTF_8);
            int bom = rawText.startsWith("\uFEFF") ? 1 : 0;
            if (psiText.equals(rawText.substring(bom))) {
                return bom == 0
                        ? psiText.substring(0, charOffset).getBytes(StandardCharsets.UTF_8).length
                        : psiText.substring(0, charOffset).getBytes(StandardCharsets.UTF_8).length
                                + "\uFEFF".getBytes(StandardCharsets.UTF_8).length;
            }
            int bytes = bom == 0 ? 0 : "\uFEFF".getBytes(StandardCharsets.UTF_8).length;
            int chars = 0;
            int at = bom;
            while (at < rawText.length() && chars < charOffset) {
                int codePoint = rawText.codePointAt(at);
                int rawLength = Character.charCount(codePoint);
                int psiLength = rawLength;
                if (codePoint == '\r') {
                    // \r\n collapses to one '\n'; a lone \r becomes '\n' as well.
                    boolean paired = at + 1 < rawText.length() && rawText.charAt(at + 1) == '\n';
                    psiLength = paired ? 0 : 1;
                }
                chars += psiLength;
                bytes += utf8Length(codePoint);
                at += rawLength;
            }
            return bytes;
        }

        private static int utf8Length(int codePoint) {
            if (codePoint < 0x80) {
                return 1;
            }
            if (codePoint < 0x800) {
                return 2;
            }
            return codePoint < 0x10000 ? 3 : 4;
        }
    }
}
