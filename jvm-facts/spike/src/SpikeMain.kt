@file:OptIn(KaExperimentalApi::class)

/**
 * Track B spike: can a headless program boot a K2 StandaloneAnalysisAPISession
 * over source strings/files and answer, for a call site:
 *
 *   1. the resolved callee (fully-qualified callable id + JVM owner),
 *   2. parameter and result types (KaType renderings),
 *   3. the erased JVM descriptors,
 *
 * plus the expression type of the call expression itself.
 *
 * This is a spike, not an integration: it prints and exits. No protocol, no
 * JSON, no wiring into the Rust provider. See docs/analysis-api-spike.md.
 */

import com.intellij.openapi.util.Disposer
import com.intellij.psi.PsiElement
import com.intellij.psi.util.PsiTreeUtil
import org.jetbrains.kotlin.analysis.api.KaExperimentalApi
import org.jetbrains.kotlin.analysis.api.analyze
import org.jetbrains.kotlin.analysis.api.projectStructure.KaSourceModule
import org.jetbrains.kotlin.analysis.api.renderer.render
import org.jetbrains.kotlin.analysis.api.resolution.successfulFunctionCallOrNull
import org.jetbrains.kotlin.analysis.api.standalone.buildStandaloneAnalysisAPISession
import org.jetbrains.kotlin.analysis.project.structure.builder.buildKtLibraryModule
import org.jetbrains.kotlin.analysis.project.structure.builder.buildKtSourceModule
import org.jetbrains.kotlin.platform.jvm.JvmPlatforms
import org.jetbrains.kotlin.psi.KtCallExpression
import org.jetbrains.kotlin.psi.KtFile
import org.jetbrains.kotlin.types.Variance
import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.Paths

private fun elapsedMs(startNanos: Long): Long = (System.nanoTime() - startNanos) / 1_000_000

private fun memory(): String {
    val rt = Runtime.getRuntime()
    return "used=${(rt.totalMemory() - rt.freeMemory()) / 1_048_576}MiB max=${rt.maxMemory() / 1_048_576}MiB"
}

private fun lineOf(element: PsiElement): Int {
    val text = element.containingFile.text
    return text.take(element.textOffset).count { it == '\n' } + 1
}

private fun columnOf(element: PsiElement): Int {
    val text = element.containingFile.text
    val lineStart = text.lastIndexOf('\n', element.textOffset - 1) + 1
    return element.textOffset - lineStart + 1
}

fun main(args: Array<String>) {
    val home = Paths.get(System.getProperty("spike.home") ?: ".").toAbsolutePath().normalize()
    val srcRoot: Path = Paths.get(args.getOrNull(0) ?: home.resolve("fixtures/src/main").toString())
    val stdlib: Path = Paths.get(args.getOrNull(1) ?: home.resolve("libs/kotlin-stdlib-2.3.21.jar").toString())
    // Third arg: callee name to report on, or "*" for every call expression.
    val calleeFilter: String = args.getOrNull(2) ?: "add"

    println("spike    : java=${System.getProperty("java.version")} vm=${System.getProperty("java.vm.name")}")
    println("spike    : srcRoot=$srcRoot")
    println("spike    : stdlib=$stdlib")
    require(Files.isDirectory(srcRoot)) { "missing fixture dir $srcRoot" }
    require(Files.isRegularFile(stdlib)) { "missing stdlib jar $stdlib (run ../fetch-deps.sh)" }

    val t0 = System.nanoTime()
    val disposable = Disposer.newDisposable()
    var mainModuleRef: KaSourceModule? = null
    val session = buildStandaloneAnalysisAPISession(disposable) {
        buildKtModuleProvider {
            platform = JvmPlatforms.defaultJvmPlatform
            val stdlibModule = buildKtLibraryModule {
                libraryName = "kotlin-stdlib"
                platform = JvmPlatforms.defaultJvmPlatform
                addBinaryRoot(stdlib)
            }
            addModule(stdlibModule)
            val mainModule = buildKtSourceModule {
                moduleName = "spike-main"
                platform = JvmPlatforms.defaultJvmPlatform
                addSourceRoot(srcRoot)
                addRegularDependency(stdlibModule)
            }
            addModule(mainModule)
            mainModuleRef = mainModule
        }
    }
    println("phase    : session-build ${elapsedMs(t0)} ms  ${memory()}")

    println(
        "phase    : modules " +
            session.modulesWithFiles.keys.joinToString { "${it.name}(${session.modulesWithFiles.getValue(it).size} files)" }
    )
    val sourceModule = mainModuleRef
        ?: session.modulesWithFiles.keys.filterIsInstance<KaSourceModule>().firstOrNull()
        ?: error("session exposes no KaSourceModule")
    val ktFiles = session.modulesWithFiles[sourceModule].orEmpty().filterIsInstance<KtFile>()
    println("phase    : files-loaded ${elapsedMs(t0)} ms  ${ktFiles.map { it.name }}")

    var queries = 0
    for (file in ktFiles) {
        for (call in PsiTreeUtil.findChildrenOfType(file, KtCallExpression::class.java)) {
            if (calleeFilter != "*" && call.calleeExpression?.text != calleeFilter) continue
            queries++
            val tQuery = System.nanoTime()
            println(
                "--- call-site ${file.name}:${lineOf(call)}:${columnOf(call)} " +
                    "offset=${call.textOffset} text='${call.text}'"
            )
            analyze(call) {
                val exprType = call.expressionType
                val resolved = call.resolveToCall()?.successfulFunctionCallOrNull()
                val candidates = call.resolveToCallCandidates().size

                println("    expressionType   = ${if (exprType != null) exprType.render(position = Variance.INVARIANT) else "<null>"}")
                println(
                    "    expressionDescr  = " +
                        if (exprType != null) exprType.mapToJvmTypeDescriptor() else "<null>"
                )
                if (resolved == null) {
                    println("    resolution       = UNRESOLVED (candidates=$candidates)")
                    return@analyze
                }
                val signature = resolved.signature
                println("    calleeFqn        = ${signature.callableId?.asSingleFqName()?.asString() ?: "<local>"}")
                println("    calleeJvmOwner   = ${signature.callableId?.classId?.asFqNameString() ?: "<no classId>"}")
                println(
                    "    extensionRecv    = " +
                        (signature.receiverType?.render(position = Variance.INVARIANT) ?: "<none>")
                )
                println(
                    "    dispatchRecv     = " +
                        (resolved.dispatchReceiver?.type?.render(position = Variance.INVARIANT) ?: "<none>")
                )
                println(
                    "    parameterTypes   = [" +
                        signature.valueParameters.joinToString(", ") { "${it.name}: ${it.returnType.render(position = Variance.INVARIANT)}" } +
                        "]"
                )
                println(
                    "    returnType       = ${signature.returnType.render(position = Variance.INVARIANT)}" +
                        " (markedNullable=${signature.returnType.isMarkedNullable}" +
                        ", isNullable=${signature.returnType.isNullable}" +
                        ", flexible=${signature.returnType.hasFlexibleNullability})"
                )
                val paramDescr = signature.valueParameters.joinToString("") {
                    it.returnType.mapToJvmTypeDescriptor()
                }
                println("    methodDescr      = ($paramDescr)${signature.returnType.mapToJvmTypeDescriptor()}")
                println("    candidates       = $candidates")
            }
            println("    queryMs          = ${elapsedMs(tQuery)}")
        }
    }

    println("phase    : queries-done ${elapsedMs(t0)} ms  count=$queries  ${memory()}")
    Disposer.dispose(disposable)
    println("phase    : disposed ${elapsedMs(t0)} ms")
    // Finding recorded in docs/analysis-api-spike.md: the standalone session
    // leaves non-daemon threads alive, so a spike that merely returns from main
    // never exits. Real helpers must terminate explicitly (they are long-lived
    // stdio processes, so this is a shutdown-path requirement, not a blocker).
    println("RESULT   : " + if (queries == 0) "FAIL (no call site found)" else "OK ($queries call sites resolved)")
    kotlin.system.exitProcess(if (queries == 0) 1 else 0)
}
