import java.io.File
import org.gradle.api.DefaultTask
import org.gradle.api.GradleException
import org.gradle.api.tasks.Input
import org.gradle.api.tasks.TaskAction

open class BuildTask : DefaultTask() {
    @Input
    var rootDirRel: String? = null
    @Input
    var target: String? = null
    @Input
    var release: Boolean? = null

    @TaskAction
    fun assemble() {
        // `cargo tauri android build` compiles the native library and symlinks
        // it into jniLibs itself. This task only verifies the JNI entry point
        // the Kotlin side calls is present (UnsatisfiedLinkError guard).
        val rootDirRel = rootDirRel ?: throw GradleException("rootDirRel cannot be null")
        val target = target ?: throw GradleException("target cannot be null")
        val abi = when (target) {
            "aarch64" -> "arm64-v8a"
            "armv7" -> "armeabi-v7a"
            "i686" -> "x86"
            "x86_64" -> "x86_64"
            else -> target
        }
        // project.projectDir is the app module dir (gen/android/app); the
        // Rust .so that `cargo tauri android build` symlinked lives under
        // src/main/jniLibs in it.
        val jniLibs = File(project.projectDir, "src/main/jniLibs/$abi")
        val lib = File(jniLibs, "libmausfer_android.so")
        if (!lib.exists()) {
            throw GradleException("libmausfer_android.so not found at $lib")
        }
        // JNI symbol must be present; otherwise the app crashes on launch.
        val marker = "Java_com_mausfer_app_Rust_create"
        val hasSymbol = ProcessBuilder("strings", lib.absolutePath)
            .redirectErrorStream(true)
            .start()
            .inputStream.bufferedReader().useLines { it.any { ln -> ln.contains(marker) } }
        if (!hasSymbol) {
            throw GradleException("$lib is missing JNI symbol $marker")
        }
    }
}
