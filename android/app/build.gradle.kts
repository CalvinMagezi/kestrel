plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

// The fxh binary is an input: pass -Pfx.bin=/absolute/path/to/fxh (aarch64-linux-android build).
val fxBin = providers.gradleProperty("fx.bin")
val generated = layout.buildDirectory.dir("generated/fx")

// One version for everything: the workspace version in Cargo.toml. The release pipeline checks
// that the tag matches it.
val workspaceVersion = Regex("""(?s)\[workspace\.package\].*?version\s*=\s*"(\d+)\.(\d+)\.(\d+)"""")
    .find(file("../../Cargo.toml").readText())?.destructured
    ?: error("no [workspace.package] version in Cargo.toml")
val (vMajor, vMinor, vPatch) = workspaceVersion.toList().map { it.toInt() }

val stageBinary by tasks.registering(Copy::class) {
    into(generated)
    fxBin.orNull?.let { path ->
        require(File(path).isAbsolute && File(path).exists()) { "-Pfx.bin=$path is not an existing absolute path" }
        from(path) { into("jniLibs/arm64-v8a"); rename { "libfxh.so" } }
    }
}

android {
    namespace = "dev.fxharness.app"
    compileSdk = 36
    defaultConfig {
        applicationId = "dev.fxharness.app"
        minSdk = 28
        targetSdk = 35
        versionCode = vMajor * 10_000 + vMinor * 100 + vPatch
        versionName = "$vMajor.$vMinor.$vPatch"
        ndk { abiFilters += "arm64-v8a" }
    }
    sourceSets["main"].jniLibs.srcDir(generated.map { it.dir("jniLibs") })
    packaging {
        // fxh is an executable run from nativeLibraryDir, so it must be unpacked on install.
        jniLibs.useLegacyPackaging = true
    }
    // Release signing comes from the environment, never from a file in the repository. Every
    // release must be signed with the same key or Android refuses to install it as an update.
    signingConfigs {
        providers.environmentVariable("KESTREL_KEYSTORE_FILE").orNull?.let { path ->
            create("release") {
                storeFile = file(path)
                storePassword = providers.environmentVariable("KESTREL_KEYSTORE_PASSWORD").get()
                keyAlias = providers.environmentVariable("KESTREL_KEY_ALIAS").get()
                keyPassword = providers.environmentVariable("KESTREL_KEY_PASSWORD").get()
            }
        }
    }
    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.findByName("release")
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

kotlin { jvmToolchain(17) }

tasks.named("preBuild") { dependsOn(stageBinary) }
