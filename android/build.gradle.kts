// Raíz Gradle: sólo declara plugins versionados por el catálogo; el módulo app
// los aplica. Versiones fijas, sin rangos ni `latest`.
plugins {
    alias(libs.plugins.android.application) apply false
    alias(libs.plugins.kotlin.android) apply false
    alias(libs.plugins.kotlin.compose) apply false
}
