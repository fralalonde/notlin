package notlin.facts.kotlin;

import org.jetbrains.kotlin.analysis.api.KaSession;
import org.jetbrains.kotlin.analysis.api.AnalyzeKt;
import org.jetbrains.kotlin.analysis.api.resolution.KaCallInfoKt;
import org.jetbrains.kotlin.analysis.api.resolution.KaFunctionCall;
import org.jetbrains.kotlin.analysis.api.signatures.KaFunctionSignature;
import org.jetbrains.kotlin.analysis.api.signatures.KaVariableSignature;
import org.jetbrains.kotlin.analysis.api.symbols.KaConstructorSymbol;
import org.jetbrains.kotlin.analysis.api.symbols.KaPropertySymbol;
import org.jetbrains.kotlin.analysis.api.symbols.KaSymbol;
import org.jetbrains.kotlin.analysis.api.symbols.KaSymbolLocationKt;
import org.jetbrains.kotlin.analysis.api.symbols.KaValueParameterSymbol;
import org.jetbrains.kotlin.analysis.api.types.KaErrorType;
import org.jetbrains.kotlin.analysis.api.types.KaFlexibleType;
import org.jetbrains.kotlin.analysis.api.types.KaType;
import org.jetbrains.kotlin.analysis.api.standalone.StandaloneAnalysisAPISession;
import org.jetbrains.kotlin.lexer.KtTokens;
import org.jetbrains.kotlin.name.CallableId;
import org.jetbrains.kotlin.psi.KtBinaryExpression;
import org.jetbrains.kotlin.psi.KtCallExpression;
import org.jetbrains.kotlin.psi.KtFile;
import org.jetbrains.kotlin.psi.KtImportDirective;
import org.jetbrains.kotlin.psi.KtNameReferenceExpression;
import org.jetbrains.kotlin.psi.KtNamedDeclaration;
import org.jetbrains.kotlin.psi.KtProperty;
import org.jetbrains.kotlin.psi.KtQualifiedExpression;
import org.jetbrains.kotlin.psi.KtUnaryExpression;
import org.jetbrains.kotlin.types.Variance;
import com.intellij.psi.PsiElement;
import com.intellij.psi.util.PsiTreeUtil;

import kotlin.jvm.functions.Function1;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

/**
 * One {@code ResolveCalls} batch: every call expression and every source-property
 * read in a file, resolved in a single {@code analyze(file) { }} block (one FIR
 * acquire per file instead of one per site).
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
 *
 * <h2>Source-property references (additive {@code references} field)</h2>
 *
 * The same analysis block also answers, for every <em>read</em> of an identifier,
 * whether K2 binds it to a top-level Kotlin property declared in a {@code .kt}
 * file of this session. The answer is semantic, never a spelling match: an
 * identifier that resolves to a local, a parameter, an inherited or declared
 * member, an extension property, a package segment, a classifier, a function call
 * or an unresolved name produces no entry at all. Absence is the only way to say
 * "not proven"; there is no fallback that guesses a target from the identifier
 * text.
 *
 * <ul>
 *   <li>{@code byte_offset} / {@code end_byte} — UTF-8 byte span of the reference
 *       itself in the requested file's on-disk bytes (the identical
 *       {@link Offsets} mapping the call sites use, so non-ASCII and CRLF files
 *       map to what the Rust lexer sees).</li>
 *   <li>{@code text} — the reference exactly as spelled in the source, including
 *       backticks ({@code `quoted name`}) and an imported alias ({@code TOKEN}).</li>
 *   <li>{@code target_file} — canonical, absolute path of the declared
 *       {@code .kt} file (the same real-path form the session indexes by, so it
 *       compares equal to a query path).</li>
 *   <li>{@code target_start_byte} / {@code target_end_byte} — UTF-8 byte span of
 *       the <em>complete</em> {@code KtProperty} declaration in that file, mapped
 *       through the same offset walk. The target file's own bytes are read and
 *       verified to agree with the PSI text first; when they do not (the file
 *       changed on disk after the session read it) no entry is emitted.</li>
 *   <li>{@code target_name} — K2 semantic name, unescaped
 *       ({@code quoted name} for {@code `quoted name`}, {@code KEY} for an aliased
 *       {@code TOKEN}).</li>
 *   <li>{@code target_package} — the semantic package of the declaration.</li>
 *   <li>{@code target_jvm_descriptor} — erased JVM descriptor of the resolved
 *       symbol's <em>declaration</em> return type ({@code Ljava/lang/String;},
 *       {@code I}), never the type of the reading expression: a
 *       {@code val WIDE: Any = "s"} read reports {@code Ljava/lang/Object;}.
 *       {@code null} means Unknown (the declaration type could not be mapped);
 *       it is never a guess.</li>
 *   <li>{@code target_nullable} — true when the declared type is marked nullable
 *       (or flexibly nullable), false only for a provably non-null declaration,
 *       {@code null} when Unknown. Descriptors are erased, so this is the only
 *       nullability signal on the wire.</li>
 * </ul>
 *
 * Write positions (assignment targets, {@code ++}/{@code --} operands) and import
 * directive names are not reads, so they produce no entry even when the resolved
 * property is top-level.
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

    /**
     * One identifier read bound to a source-backed top-level Kotlin property.
     * Offsets are UTF-8 byte offsets into the on-disk bytes, never PSI char
     * offsets.
     */
    record Reference(
            int byteOffset,
            int endByte,
            String text,
            String targetFile,
            int targetStartByte,
            int targetEndByte,
            String targetName,
            String targetPackage,
            String targetJvmDescriptor,
            Boolean targetNullable) {
    }

    /** Everything one {@code ResolveCalls} request answers for a file. */
    record Batch(List<Site> sites, List<Reference> references) {

        static final Batch EMPTY = new Batch(List.of(), List.of());
    }

    static final String ESTABLISHED = "Established";
    static final String AMBIGUOUS = "Ambiguous";
    static final String UNKNOWN = "Unknown";

    /**
     * Resolves every call expression and every source-property read in
     * {@code file} inside one analysis session.
     *
     * @throws RuntimeException when the session or a type provider fails outright;
     *         the caller drops the whole batch instead of answering partially.
     */
    @SuppressWarnings("deprecation") // analyze(KtElement, ...) is what the spike validated; the
                                     // non-deprecated overload takes a KaModule, not a file.
    static Batch extractAll(StandaloneAnalysisAPISession session, KtFile file, byte[] raw) {
        List<KtCallExpression> calls = new ArrayList<>(PsiTreeUtil.findChildrenOfType(file, KtCallExpression.class));
        List<KtNameReferenceExpression> reads = new ArrayList<>();
        for (KtNameReferenceExpression expression
                : PsiTreeUtil.findChildrenOfType(file, KtNameReferenceExpression.class)) {
            if (isReadPosition(expression)) {
                reads.add(expression);
            }
        }
        if (calls.isEmpty() && reads.isEmpty()) {
            return Batch.EMPTY;
        }
        String psiText = file.getText();
        // One decode for the whole batch. The requested-file mapping is only
        // usable when the bytes read for this request still describe the text
        // the session analyzed: otherwise the PSI (and every binding resolved
        // from it) belongs to an earlier revision.
        Offsets requestedOffsets = Offsets.of(raw, psiText);
        boolean requestedFresh = requestedOffsets.agreesWithDisk();
        ByteMaps targetMaps = new ByteMaps(canonicalPath(file), requestedOffsets, requestedFresh);
        return AnalyzeKt.analyze(file, (Function1<KaSession, Batch>) analysis -> {
            List<Site> sites = new ArrayList<>(calls.size());
            for (KtCallExpression call : calls) {
                sites.add(site(analysis, call, raw, psiText));
            }
            if (!requestedFresh) {
                // A same-width edit can change which symbol an identifier binds
                // to while keeping its text and offset, so a stale binding is
                // indistinguishable from a current one on the wire. No
                // reference is the only honest answer here; call sites keep
                // their existing (raw-offset) behavior.
                return new Batch(List.copyOf(sites), List.of());
            }
            List<Reference> references = new ArrayList<>(reads.size());
            for (KtNameReferenceExpression read : reads) {
                Reference reference = reference(analysis, read, requestedOffsets, targetMaps);
                if (reference != null) {
                    references.add(reference);
                }
            }
            references.sort(Comparator.comparingInt(Reference::byteOffset));
            return new Batch(List.copyOf(sites), List.copyOf(references));
        });
    }

    /**
     * One identifier read, or {@code null} when it is not a unique, source-backed,
     * top-level Kotlin property. Every rejection is a "no evidence" answer: the
     * batch never substitutes a spelling-based guess.
     */
    private static Reference reference(KaSession analysis, KtNameReferenceExpression expression,
            Offsets requested, ByteMaps targetMaps) {
        // getTextRange() for both ends: Kotlin declarations override
        // getTextOffset() to answer the *name identifier* offset, so a span taken
        // from it would silently start after the modifiers. Using one API for both
        // the reference and the declared span keeps every offset comparable.
        int byteOffset = requested.byteOffset(expression.getTextRange().getStartOffset());
        int endByte = requested.byteOffset(expression.getTextRange().getEndOffset());
        String text = expression.getText();
        KaSymbol symbol;
        try {
            // A plain identifier read resolves to at most one symbol; an
            // unresolved name answers null, which is not an error but no evidence.
            symbol = analysis.resolveSymbol(expression);
        } catch (RuntimeException | LinkageError resolutionFailure) {
            return null;
        }
        if (!(symbol instanceof KaPropertySymbol property)) {
            // Locals, parameters, classifiers, package segments and functions are
            // simply not properties.
            return null;
        }
        if (property.isExtension()) {
            // A receiver-bearing read is not a plain field reference.
            return null;
        }
        if (!KaSymbolLocationKt.isTopLevel(property)) {
            return null;
        }
        CallableId callableId = property.getCallableId();
        if (callableId == null || callableId.getClassId() != null) {
            return null;
        }
        // Source identity comes from the resolved symbol's PSI, not from matching
        // declaration text: an object member, a primary-constructor property, a
        // local and a top-level property can all spell the same name.
        if (!(property.getPsi() instanceof KtProperty declaration) || declaration.isLocal()) {
            return null;
        }
        if (!(declaration.getParent() instanceof KtFile declarationFile)) {
            return null;
        }
        String targetFile = canonicalPath(declarationFile);
        if (targetFile == null) {
            return null;
        }
        Offsets mapping = targetMaps.forFile(targetFile, declarationFile);
        if (mapping == null) {
            // The declaring file's on-disk bytes are unavailable or no longer
            // agree with the analyzed text: offsets would be a guess.
            return null;
        }
        int targetStart = mapping.byteOffset(declaration.getTextRange().getStartOffset());
        int targetEnd = mapping.byteOffset(declaration.getTextRange().getEndOffset());
        // Declared-type evidence, taken from the resolved top-level symbol's own
        // return type. A reading expression's type may be narrower than the
        // declaration (`val WIDE: Any = "s"` reads as String), so using it here
        // would describe the use site instead of the field the consumer emits.
        String targetDescriptor = null;
        Boolean targetNullable = null;
        try {
            KaType declared = property.getReturnType();
            if (!isError(declared)) {
                targetDescriptor = analysis.mapToJvmTypeDescriptor(declared);
                targetNullable = isNullable(analysis, declared);
            }
        } catch (RuntimeException | LinkageError unmappable) {
            // Keep the binding metadata; the new fields stay Unknown.
            targetDescriptor = null;
            targetNullable = null;
        }
        return new Reference(byteOffset, endByte, text, targetFile, targetStart, targetEnd,
                callableId.getCallableName().asString(), callableId.getPackageName().asString(),
                targetDescriptor, targetNullable);
    }

    /**
     * Cheap PSI gate applied before any resolution, so the batch does not pay K2
     * costs for positions that can never be a property read.
     */
    private static boolean isReadPosition(KtNameReferenceExpression expression) {
        if (PsiTreeUtil.getParentOfType(expression, KtImportDirective.class) != null) {
            // `import a.b.KEY` resolves to the property, but an import name is not
            // a value read; a consumer must never be told to rewrite it.
            return false;
        }
        if (isWriteTarget(expression)) {
            return false;
        }
        // A declaration's own name identifier is not a reference to itself. Kotlin
        // PSI does not model it as a KtNameReferenceExpression today; the guard
        // keeps a future PSI shape from turning a declaration into a self-read.
        KtNamedDeclaration declaration = PsiTreeUtil.getParentOfType(expression, KtNamedDeclaration.class);
        return declaration == null || declaration.getNameIdentifier() != expression;
    }

    /**
     * True when the identifier is the target of an assignment
     * ({@code KEY = …}, {@code KEY += …}, {@code ++KEY}, {@code KEY--}), including
     * a qualified target whose selector chain ends at it. A write is not a read:
     * emitting one would invite a consumer to substitute a value reference where
     * the program stores.
     */
    private static boolean isWriteTarget(KtNameReferenceExpression expression) {
        PsiElement target = expression;
        while (target.getParent() instanceof KtQualifiedExpression qualified
                && qualified.getSelectorExpression() == target) {
            target = qualified;
        }
        PsiElement parent = target.getParent();
        if (parent instanceof KtBinaryExpression binary) {
            return binary.getLeft() == target
                    && binary.getOperationReference() != null
                    && KtTokens.ALL_ASSIGNMENTS.contains(
                            binary.getOperationReference().getReferencedNameElementType());
        }
        if (parent instanceof KtUnaryExpression unary) {
            return unary.getBaseExpression() == target
                    && KtTokens.INCREMENT_AND_DECREMENT.contains(unary.getOperationToken());
        }
        return false;
    }

    /** Canonical real path of a PSI file, or {@code null} when it has none. */
    private static String canonicalPath(KtFile file) {
        if (file.getVirtualFile() == null || !file.getVirtualFile().isValid()) {
            return null;
        }
        try {
            return Paths.get(file.getVirtualFile().getPath()).toRealPath().toString();
        } catch (IOException | RuntimeException missing) {
            return null;
        }
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
    static String toAnswerLine(long id, String file, Batch batch) {
        List<Site> sites = batch.sites();
        List<Reference> references = batch.references();
        StringBuilder line = new StringBuilder(64 + sites.size() * 192 + references.size() * 192);
        line.append("{\"version\":1,\"id\":").append(id).append(",\"answer\":{\"CallSites\":{\"file\":");
        appendString(line, file);
        line.append(",\"sites\":[");
        for (int index = 0; index < sites.size(); index++) {
            if (index > 0) {
                line.append(',');
            }
            appendSite(line, sites.get(index));
        }
        // Additive field: a consumer that does not know it must ignore it, and an
        // older helper that omits it means "no reference evidence at all".
        line.append("],\"references\":[");
        for (int index = 0; index < references.size(); index++) {
            if (index > 0) {
                line.append(',');
            }
            appendReference(line, references.get(index));
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

    private static void appendReference(StringBuilder out, Reference reference) {
        out.append("{\"byte_offset\":").append(reference.byteOffset());
        out.append(",\"end_byte\":").append(reference.endByte());
        out.append(",\"text\":");
        appendString(out, reference.text());
        out.append(",\"target_file\":");
        appendString(out, reference.targetFile());
        out.append(",\"target_start_byte\":").append(reference.targetStartByte());
        out.append(",\"target_end_byte\":").append(reference.targetEndByte());
        out.append(",\"target_name\":");
        appendString(out, reference.targetName());
        out.append(",\"target_package\":");
        appendString(out, reference.targetPackage());
        // Additive declared-type evidence: null means Unknown, so a consumer that
        // needs an exact field type (a bounded constant reference) refuses rather
        // than guessing from the reading expression.
        out.append(",\"target_jvm_descriptor\":");
        appendNullableString(out, reference.targetJvmDescriptor());
        out.append(",\"target_nullable\":");
        if (reference.targetNullable() == null) {
            out.append("null");
        } else {
            out.append(reference.targetNullable().booleanValue());
        }
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

    /**
     * Source bytes of the file a reference points at, mapped to the PSI text the
     * session analyzed.
     *
     * <p>Only files whose bytes still agree with the analyzed text are usable: the
     * Analysis API answers for files on disk, while notlin plans on virtual
     * snapshots, so a declaring file can have changed since the session read it.
     * A stale or unreadable file is cached as "unavailable" and produces no
     * entries rather than offsets computed against the wrong bytes.
     */
    private static final class ByteMaps {

        private final Map<String, Offsets> byPath = new HashMap<>();

        ByteMaps(String requestedPath, Offsets requested, boolean requestedFresh) {
            if (requestedPath != null) {
                // Reuse the single decode of the requested file's bytes instead
                // of re-reading (or re-decoding) the disk per reference — but
                // only when those bytes still describe the analyzed PSI. A
                // stale mapping would silently make the session's old revision
                // addressable; caching null keeps the answer "unavailable" for
                // the rest of the batch.
                byPath.put(requestedPath, requestedFresh ? requested : null);
            }
        }

        Offsets forFile(String canonicalPath, KtFile file) {
            if (byPath.containsKey(canonicalPath)) {
                return byPath.get(canonicalPath);
            }
            Offsets mapping;
            try {
                mapping = Offsets.of(Files.readAllBytes(Path.of(canonicalPath)), file.getText());
                if (!mapping.agreesWithDisk()) {
                    mapping = null;
                }
            } catch (IOException | RuntimeException unreadable) {
                mapping = null;
            }
            byPath.put(canonicalPath, mapping);
            return mapping;
        }
    }

    /**
     * PSI char offset -> UTF-8 byte offset in the on-disk file.
     *
     * <p>Decodes the raw bytes once, so a batch can map many offsets of the same
     * file without re-decoding it per site.
     */
    static final class Offsets {

        private final String rawText;
        private final String psiText;
        private final int bom;

        private Offsets(String rawText, String psiText) {
            this.rawText = rawText;
            this.psiText = psiText;
            this.bom = rawText.startsWith("\uFEFF") ? 1 : 0;
        }

        static Offsets of(byte[] raw, String psiText) {
            return new Offsets(new String(raw, StandardCharsets.UTF_8), psiText);
        }

        /**
         * True when the PSI text and the raw bytes differ by nothing the offset
         * walk fails to model (a leading BOM and {@code \r\n}/{@code \r} line
         * separators). Anything else means the bytes on disk are a different
         * revision, where a char offset is not addressable.
         */
        boolean agreesWithDisk() {
            if (psiText.equals(rawText.substring(bom))) {
                return true;
            }
            if (rawText.indexOf('\r') < 0) {
                return false;
            }
            StringBuilder normalized = new StringBuilder(rawText.length());
            for (int at = bom; at < rawText.length(); at++) {
                char current = rawText.charAt(at);
                if (current == '\r') {
                    normalized.append('\n');
                    if (at + 1 < rawText.length() && rawText.charAt(at + 1) == '\n') {
                        at++;
                    }
                } else {
                    normalized.append(current);
                }
            }
            return psiText.contentEquals(normalized);
        }

        /**
         * The PSI text is a <em>normalized</em> view of the file (IntelliJ rewrites
         * CRLF and a leading BOM), while the Rust lexer's offsets are byte offsets
         * into the bytes on disk. When the two texts agree the mapping is a plain
         * UTF-8 prefix length; otherwise the file is walked code point by code
         * point, counting the bytes that are really there and the characters PSI
         * would have produced.
         */
        int byteOffset(int charOffset) {
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

        /** PSI char offset -> UTF-8 byte offset in the on-disk file. */
        static int byteOffset(byte[] raw, String psiText, int charOffset) {
            return of(raw, psiText).byteOffset(charOffset);
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
