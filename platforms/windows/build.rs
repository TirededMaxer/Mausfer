fn main() {
    // Default attributes: tauri-build embeds the standard Windows application
    // manifest (Common-Controls v6 dependency, DPI awareness, asInvoker UAC).
    // The previous `new_without_app_manifest()` caused a missing
    // `TaskDialogIndirect` entry point on Windows because comctl32 v5 was
    // loaded instead of v6. Modern llvm-rc (LLVM >= 18) parses the XML
    // manifest file fine for cross-compilation.
    let attributes = tauri_build::Attributes::new();
    tauri_build::try_build(attributes).expect("failed to run tauri-build");
}
