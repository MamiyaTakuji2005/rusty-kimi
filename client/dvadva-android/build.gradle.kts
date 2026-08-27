// Root build file — per-module configuration lives in `app/build.gradle.kts`.
//
// The version pins live in `gradle/libs.versions.toml`. This project is not
// part of the Cargo workspace (it contains no Rust); it is built with Android
// Studio or a standalone Gradle installation (see README.md — the wrapper jar
// is not committed and must be generated once with `gradle wrapper`).

plugins {
    alias(libs.plugins.android.application) apply false
    alias(libs.plugins.kotlin.android) apply false
    alias(libs.plugins.kotlin.compose) apply false
    alias(libs.plugins.kotlin.serialization) apply false
}
