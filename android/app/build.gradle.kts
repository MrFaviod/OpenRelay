// Módulo app de openrelay (base T12). Sin audio, sin JNI y sin backend: sólo
// una Activity Compose que muestra el placeholder. NDK/CMake/ABIs quedan
// fijados para que T13 añada el puente nativo sin renegociar versiones.
plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.compose)
}

android {
    namespace = "dev.openrelay.app"
    compileSdk = 35
    buildToolsVersion = "35.0.0"
    ndkVersion = "27.2.12479018"

    defaultConfig {
        applicationId = "dev.openrelay.app"
        minSdk = 28
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"

        // Runner listo para T14; las dependencias de test están fijadas abajo.
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"

        // STL compartida: Oboe 1.11 exige c++_shared en el AAR Prefab.
        externalNativeBuild {
            cmake {
                arguments += listOf("-DANDROID_STL=c++_shared")
            }
        }

        // Sólo estas dos ABIs, también para el futuro código nativo de T13.
        ndk {
            abiFilters += listOf("arm64-v8a", "x86_64")
        }
    }

    buildTypes {
        release {
            // Sin minify en T12: no hay reglas ni backend que ofuscar.
            isMinifyEnabled = false
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

    // Puente Rust de F2-10c: android/rust/build.sh deja las .so aquí.
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

    testImplementation(libs.junit)

    androidTestImplementation(libs.androidx.test.ext.junit)
    androidTestImplementation(libs.androidx.test.espresso.core)
    androidTestImplementation(libs.androidx.test.runner)
    androidTestImplementation(platform(libs.androidx.compose.bom))
    androidTestImplementation(libs.androidx.compose.ui.test.junit4)

    debugImplementation(libs.androidx.compose.ui.tooling)
    debugImplementation(libs.androidx.compose.ui.test.manifest)
}
