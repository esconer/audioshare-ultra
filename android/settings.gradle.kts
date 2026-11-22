pluginManagement {
    repositories {
        google()
        // Google's official Maven Central mirror avoids HTTP 429 rate-limiting
        // on shared GitHub Actions runners.
        maven { url = uri("https://maven-central.storage-download.googleapis.com/maven2") }
        mavenCentral()
        gradlePluginPortal()
    }
}
dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        // Google's official Maven Central mirror avoids HTTP 429 rate-limiting
        // from repo.maven.apache.org on shared GitHub Actions runners.
        maven { url = uri("https://maven-central.storage-download.googleapis.com/maven2") }
        mavenCentral()
        maven { url = uri("https://maven.google.com") }
    }
}
rootProject.name = "AudioShareUltra"
include(":app")
