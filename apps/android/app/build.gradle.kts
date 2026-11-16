import javax.inject.Inject

plugins {
    alias(libs.plugins.android.application)
}

/** Root of the Cargo workspace (two levels above apps/android). */
val repoRoot: File = rootDir.parentFile.parentFile

/** `cargo` from PATH, or rustup's default location. */
val cargoBin: String = System.getenv("CARGO")
    ?: File(System.getProperty("user.home"), ".cargo/bin/cargo").takeIf { it.exists() }?.path
    ?: "cargo"

/** Android ABIs we ship; the Rust core is built for each. */
val shippedAbis = listOf("arm64-v8a", "armeabi-v7a", "x86_64")

/** Builds the Rust client core (crates/ib-ffi) for every ABI with cargo-ndk. */
abstract class RustCoreTask @Inject constructor(private val exec: ExecOperations) : DefaultTask() {
    @get:OutputDirectory abstract val jniLibsDir: DirectoryProperty
    @get:Input abstract val cargo: Property<String>
    @get:Input abstract val abis: ListProperty<String>
    @get:Input abstract val profile: Property<String>
    @get:Internal abstract val workspace: DirectoryProperty

    init {
        // Cargo does its own incremental builds; always let it decide.
        outputs.upToDateWhen { false }
    }

    @TaskAction
    fun build() {
        val args = mutableListOf(cargo.get(), "ndk")
        abis.get().forEach { args += listOf("-t", it) }
        args += listOf("-P", "30", "-o", jniLibsDir.get().asFile.path, "build", "-p", "ib-ffi", "--lib")
        if (profile.get() == "release") args += "--release"
        exec.exec {
            workingDir = workspace.get().asFile
            commandLine(args)
        }
    }
}

/** Generates the Kotlin bindings from the built library with UniFFI. */
abstract class UniffiBindingsTask @Inject constructor(private val exec: ExecOperations) : DefaultTask() {
    @get:InputDirectory abstract val jniLibsDir: DirectoryProperty
    @get:OutputDirectory abstract val outDir: DirectoryProperty
    @get:Input abstract val cargo: Property<String>
    @get:Internal abstract val workspace: DirectoryProperty

    @TaskAction
    fun generate() {
        val lib = jniLibsDir.get().file("arm64-v8a/libib_ffi.so").asFile
        val ws = workspace.get().asFile
        exec.exec {
            workingDir = ws
            commandLine(
                cargo.get(), "run", "-q", "-p", "ib-ffi", "--features", "bindgen", "--bin", "uniffi-bindgen", "--",
                "generate", "--library", lib.path, "--language", "kotlin", "--no-format",
                "--out-dir", outDir.get().asFile.path,
            )
        }
    }
}

android {
    namespace = "org.inkbridge.android"
    compileSdk = 36

    defaultConfig {
        applicationId = "org.inkbridge.android"
        minSdk = 30
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
        ndk { abiFilters += shippedAbis }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

androidComponents {
    onVariants { variant ->
        val name = variant.name.replaceFirstChar { it.uppercase() }
        // The Rust core is always optimised: latency matters in debug builds too.
        val profile = "release"
        val rust = tasks.register<RustCoreTask>("buildRustCore$name") {
            cargo.set(cargoBin)
            abis.set(shippedAbis)
            this.profile.set(profile)
            workspace.set(repoRoot)
        }
        val bindings = tasks.register<UniffiBindingsTask>("generateUniffiBindings$name") {
            jniLibsDir.set(rust.flatMap { it.jniLibsDir })
            cargo.set(cargoBin)
            workspace.set(repoRoot)
        }
        variant.sources.jniLibs?.addGeneratedSourceDirectory(rust, RustCoreTask::jniLibsDir)
        variant.sources.kotlin?.addGeneratedSourceDirectory(bindings, UniffiBindingsTask::outDir)
    }
}

dependencies {
    implementation(libs.androidx.activity)
    implementation(libs.camera.camera2)
    implementation(libs.camera.lifecycle)
    implementation(libs.camera.view)
    implementation(libs.zxing.core)
    implementation(libs.graphics.core)
    // UniFFI's Kotlin bindings call into the Rust library through JNA.
    implementation("${libs.jna.get().module}:${libs.jna.get().version}@aar")
    testImplementation(libs.junit)
}
