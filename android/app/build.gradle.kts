// Módulo app de openrelay. Sin audio, sin JNI y sin backend: sólo
// una Activity Compose que muestra el placeholder. NDK/CMake/ABIs quedan
// fijados para que el puente nativo se añada sin renegociar versiones.
import java.util.Properties

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.compose)
}

// --- Release signing -------------------------------------------------------
// The release keystore is a LOCAL DEVELOPMENT key: it only makes the release
// APK installable and verifiable. It is not an upload key for Play Store or
// any other store, and it is never committed (.gitignore covers *.keystore).
//
// Values are read from environment variables first, then from the ignored
// file android/release-signing.properties. Every value must be present; if
// any is missing the release build fails instead of shipping an unsigned APK
// or silently falling back to the debug key.
val releaseSigningValues = Properties().apply {
    val localFile = rootProject.file("release-signing.properties")
    if (localFile.isFile) {
        localFile.inputStream().use { load(it) }
    }
}

fun releaseSigningValue(propertyKey: String, envKey: String): String? =
    (System.getenv(envKey) ?: releaseSigningValues.getProperty(propertyKey))
        ?.takeIf { it.isNotBlank() }

val releaseStoreFile = releaseSigningValue("storeFile", "OPENRELAY_RELEASE_STORE_FILE")
val releaseStorePassword = releaseSigningValue("storePassword", "OPENRELAY_RELEASE_STORE_PASSWORD")
val releaseKeyAlias = releaseSigningValue("keyAlias", "OPENRELAY_RELEASE_KEY_ALIAS")
val releaseKeyPassword = releaseSigningValue("keyPassword", "OPENRELAY_RELEASE_KEY_PASSWORD")
val releaseSigningReady = listOf(
    releaseStoreFile,
    releaseStorePassword,
    releaseKeyAlias,
    releaseKeyPassword,
).all { it != null }

android {
    namespace = "dev.openrelay.app"
    compileSdk = 35
    buildToolsVersion = "35.0.0"
    ndkVersion = "27.2.12479018"

    defaultConfig {
        applicationId = "dev.openrelay.app"
        minSdk = 28
        targetSdk = 35
        versionCode = 4
        versionName = "0.2.2"

        // Runner listo para la instrumentación; las dependencias de test están fijadas abajo.
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"

        // STL compartida: Oboe 1.11 exige c++_shared en el AAR Prefab.
        externalNativeBuild {
            cmake {
                arguments += listOf("-DANDROID_STL=c++_shared")
            }
        }

        // Sólo estas dos ABIs, también para el futuro código nativo.
        ndk {
            abiFilters += listOf("arm64-v8a", "x86_64")
        }
    }

    signingConfigs {
        if (releaseSigningReady) {
            create("release") {
                storeFile = rootProject.file(releaseStoreFile!!)
                storePassword = releaseStorePassword
                keyAlias = releaseKeyAlias
                keyPassword = releaseKeyPassword
            }
        }
    }

    buildTypes {
        release {
            // Sin minify: no hay reglas ni backend que ofuscar.
            isMinifyEnabled = false
            if (releaseSigningReady) {
                signingConfig = signingConfigs.getByName("release")
            }
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    buildFeatures {
        compose = true
        // Oboe llega como AAR Prefab; find_package(oboe CONFIG) lo consume.
        prefab = true
    }

    externalNativeBuild {
        cmake {
            path = file("../capture/CMakeLists.txt")
            version = "3.22.1"
        }
    }

    // Puente Rust: android/rust/build.sh deja las .so aquí.
    sourceSets["main"].jniLibs.srcDir(file("../rust/target/jniLibs"))

    packaging {
        resources {
            excludes += "/META-INF/{AL2.0,LGPL2.1}"
        }
    }
}

dependencies {
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.lifecycle.runtime.ktx)
    implementation(libs.androidx.activity.compose)

    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.compose.ui)
    implementation(libs.androidx.compose.ui.graphics)
    implementation(libs.androidx.compose.ui.tooling.preview)
    implementation(libs.androidx.compose.material3)
    implementation(libs.oboe)
    implementation(libs.zxing.android.embedded)

    testImplementation(libs.junit)

    androidTestImplementation(libs.androidx.test.ext.junit)
    androidTestImplementation(libs.androidx.test.espresso.core)
    androidTestImplementation(libs.androidx.test.runner)
    androidTestImplementation(platform(libs.androidx.compose.bom))
    androidTestImplementation(libs.androidx.compose.ui.test.junit4)

    debugImplementation(libs.androidx.compose.ui.tooling)
    debugImplementation(libs.androidx.compose.ui.test.manifest)
}

// A release build without signing material must fail loudly. AGP would
// otherwise emit app-release-unsigned.apk, which looks installable but is not.
gradle.taskGraph.whenReady {
    val buildsRelease = allTasks.any { task ->
        task.project == project &&
            task.name.startsWith("package") &&
            task.name.contains("Release")
    }
    if (buildsRelease && !releaseSigningReady) {
        throw GradleException(
            "Release signing is not configured: refusing to produce an unsigned APK. " +
                "Generate android/release.keystore (see INSTALL.md, section Android) and export " +
                "OPENRELAY_RELEASE_STORE_FILE, OPENRELAY_RELEASE_STORE_PASSWORD, " +
                "OPENRELAY_RELEASE_KEY_ALIAS and OPENRELAY_RELEASE_KEY_PASSWORD, " +
                "or write the same keys into android/release-signing.properties.",
        )
    }
}
