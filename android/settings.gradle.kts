// Proyecto Android de the reference app (base T12). Sólo módulo app; el core Rust se
// integra por JNI en T13 y no se compila desde Gradle en esta fase.
pluginManagement {
    repositories {
        google {
            content {
                includeGroupByRegex("com\\.android.*")
                includeGroupByRegex("com\\.google.*")
                includeGroupByRegex("androidx.*")
            }
        }
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    // Sin repositorios declarados en módulos: todo se resuelve aquí.
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "arl-android"
include(":app")
