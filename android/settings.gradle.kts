pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}
dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
        // Redundant mirrors: GitHub runners occasionally cache a stale
        // "module not found" from repo.maven.apache.org / dl.google.com.
        maven { url = uri("https://repo1.maven.org/maven2") }
        maven { url = uri("https://maven.google.com") }
    }
}
rootProject.name = "AudioShareUltra"
include(":app")
