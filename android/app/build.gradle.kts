plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "dev.tabdisplay"
    compileSdk = 35

    defaultConfig {
        applicationId = "dev.tabdisplay"
        minSdk = 30
        targetSdk = 34
        versionCode = 1
        versionName = "1.0"
        ndk { abiFilters += "arm64-v8a" }
    }

    buildTypes {
        release {
            // Signed with the debug key so `gradle installRelease` works out of the box.
            signingConfig = signingConfigs.getByName("debug")
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
}

dependencies {
    // Front-buffered GL rendering (draws into the buffer the panel is scanning out).
    implementation("androidx.graphics:graphics-core:1.0.3")
    testImplementation("junit:junit:4.13.2")
}

// Native hot paths (android/native, Rust) via cargo-ndk: `cargo install cargo-ndk` and
// `rustup target add aarch64-linux-android`, plus an Android NDK.
val buildNative by tasks.registering(Exec::class) {
    workingDir = file("../native")
    // Platform 30 = minSdk: libnativewindow (AHardwareBuffer) is not in older sysroots.
    commandLine("cargo", "ndk", "-t", "arm64-v8a", "-P", "30", "-o", "../app/src/main/jniLibs", "build", "--release")
}
tasks.named("preBuild") { dependsOn(buildNative) }
