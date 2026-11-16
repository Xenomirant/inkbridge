//! Generates libva bindings from the system headers when the `vaapi`
//! feature is on (Linux). libva keeps its ABI stable, so the bindings match
//! any newer libva at run time.

fn main() {
    #[cfg(feature = "vaapi")]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        vaapi();
    }
}

#[cfg(feature = "vaapi")]
fn vaapi() {
    let va = pkg_config::Config::new()
        .atleast_version("1.0")
        .probe("libva")
        .expect("libva not found (install libva-dev)");
    pkg_config::Config::new()
        .probe("libva-drm")
        .expect("libva-drm not found (install libva-dev)");
    let mut builder = bindgen::Builder::default()
        .header_contents(
            "va_wrapper.h",
            "#include <va/va.h>\n#include <va/va_drm.h>\n#include <va/va_enc_h264.h>\n",
        )
        .allowlist_function("va(Initialize|Terminate|GetDisplayDRM|QueryConfigProfiles|QueryConfigEntrypoints|GetConfigAttributes|CreateConfig|DestroyConfig|CreateSurfaces|DestroySurfaces|CreateContext|DestroyContext|CreateBuffer|DestroyBuffer|MapBuffer|UnmapBuffer|BeginPicture|RenderPicture|EndPicture|SyncSurface|DeriveImage|CreateImage|DestroyImage|PutImage|QueryVendorString|ErrorStr|MaxNumProfiles|MaxNumEntrypoints)")
        .allowlist_type("VA(EncSequenceParameterBufferH264|EncPictureParameterBufferH264|EncSliceParameterBufferH264|EncMiscParameterBuffer|EncMiscParameterRateControl|EncMiscParameterHRD|EncMiscParameterFrameRate|EncPackedHeaderParameterBuffer|CodedBufferSegment|Image|ImageFormat|ConfigAttrib|SurfaceAttrib)")
        .allowlist_var("VA_.*")
        .derive_default(true)
        .layout_tests(false)
        .generate_comments(false);
    for p in &va.include_paths {
        builder = builder.clang_arg(format!("-I{}", p.display()));
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    builder
        .generate()
        .expect("generating libva bindings (needs libclang-dev)")
        .write_to_file(out.join("va.rs"))
        .expect("writing libva bindings");
}
