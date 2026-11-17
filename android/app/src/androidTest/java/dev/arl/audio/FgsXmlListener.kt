package dev.arl.audio

import androidx.test.internal.runner.listener.InstrumentationRunListener
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.runner.Description
import org.junit.runner.Result
import org.junit.runner.notification.Failure
import java.io.File

/**
 * Un XML por invocación de `am instrument`. AndroidJUnitRunner sólo acepta
 * en `-e listener` una subclase de InstrumentationRunListener; un
 * RunListener pelado no se registra. El nombre llega con
 * `-e fgsXmlName <metodo>.xml`. Sin ese argumento no escribe nada.
 *
 * El archivo queda en el almacenamiento privado de la app, legible con
 * `run-as dev.arl.audio`. Cuenta tests, failures, errors y skipped.
 * Un assumption es skipped, nunca un PASS.
 */
class FgsXmlListener : InstrumentationRunListener() {

    private val cases = ArrayList<Case>()
    private var open: Case? = null
    private var openAt: Long = 0L

    private class Case(val name: String, val className: String) {
        var seconds: Double = 0.0
        var failure: String? = null
        var error: String? = null
        var skipped: String? = null
    }

    override fun testRunStarted(description: Description) {
        cases.clear()
    }

    override fun testStarted(description: Description) {
        open = Case(description.methodName, description.className)
        openAt = System.nanoTime()
    }

    override fun testFailure(failure: Failure) {
        val case = ensure(failure.description)
        val text = failure.trace ?: failure.message ?: failure.exception.toString()
        if (failure.exception is AssertionError) case.failure = text
        else case.error = text
    }

    override fun testAssumptionFailure(failure: Failure) {
        ensure(failure.description).skipped = failure.message ?: "assumption"
    }

    override fun testIgnored(description: Description) {
        cases.add(Case(description.methodName, description.className).also {
            it.skipped = "ignored"
        })
    }

    override fun testFinished(description: Description) {
        val case = open ?: return
        case.seconds = (System.nanoTime() - openAt) / 1_000_000_000.0
        cases.add(case)
        open = null
    }

    override fun testRunFinished(result: Result) {
        writeXml()
    }

    private fun ensure(description: Description): Case {
        open?.let { return it }
        return Case(description.methodName, description.className).also { open = it }
    }

    private fun writeXml() {
        val name = InstrumentationRegistry.getArguments().getString("fgsXmlName") ?: return
        require(name.endsWith(".xml") && !name.contains("/") && !name.contains("..")) {
            "fgsXmlName no es un nombre de archivo: $name"
        }
        val dir = InstrumentationRegistry.getInstrumentation().targetContext.filesDir
        val failures = cases.count { it.failure != null }
        val errors = cases.count { it.error != null }
        val skipped = cases.count { it.skipped != null }
        val body = buildString {
            appendLine("""<?xml version="1.0" encoding="UTF-8"?>""")
            appendLine(
                """<testsuite name="dev.arl.audio.FgsLifecycleTest" tests="${cases.size}" """ +
                    """failures="$failures" errors="$errors" skipped="$skipped">"""
            )
            for (case in cases) {
                append(
                    """  <testcase classname="${xml(case.className)}" name="${xml(case.name)}" """ +
                        """time="${"%.3f".format(case.seconds)}">"""
                )
                when {
                    case.failure != null ->
                        append(
                            "\n    <failure message=\"${xml(firstLine(case.failure))}\">" +
                                "<![CDATA[${case.failure}]]></failure>\n  "
                        )
                    case.error != null ->
                        append(
                            "\n    <error message=\"${xml(firstLine(case.error))}\">" +
                                "<![CDATA[${case.error}]]></error>\n  "
                        )
                    case.skipped != null ->
                        append("\n    <skipped message=\"${xml(case.skipped)}\"/>\n  ")
                }
                appendLine("</testcase>")
            }
            appendLine("</testsuite>")
        }
        File(dir, name).writeText(body)
    }

    private fun firstLine(text: String?): String =
        text?.lineSequence()?.firstOrNull().orEmpty()

    private fun xml(text: String?): String = text.orEmpty()
        .replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace("\"", "&quot;")
}
