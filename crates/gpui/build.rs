use std::env;

fn main() {
    let target = env::var("CARGO_CFG_TARGET_OS");
    println!("cargo::rustc-check-cfg=cfg(gles)");

    match target.as_deref() {
        Ok("macos") => {
            macos::build();
        }
        _ => (),
    };
}

mod macos {
    use std::{
        env,
        path::{Path, PathBuf},
    };

    use cbindgen::Config;

    pub(super) fn build() {
        generate_dispatch_bindings();
        let header_path = generate_shader_bindings();

        compile_metal_shaders(&header_path);
    }

    fn generate_dispatch_bindings() {
        println!("cargo:rustc-link-lib=framework=System");

        let bindings = bindgen::Builder::default()
            .header("src/platform/mac/dispatch.h")
            .allowlist_var("_dispatch_main_q")
            .allowlist_var("_dispatch_source_type_data_add")
            .allowlist_var("DISPATCH_QUEUE_PRIORITY_HIGH")
            .allowlist_var("DISPATCH_TIME_NOW")
            .allowlist_function("dispatch_get_global_queue")
            .allowlist_function("dispatch_async_f")
            .allowlist_function("dispatch_after_f")
            .allowlist_function("dispatch_time")
            .allowlist_function("dispatch_source_merge_data")
            .allowlist_function("dispatch_source_create")
            .allowlist_function("dispatch_source_set_event_handler_f")
            .allowlist_function("dispatch_resume")
            .allowlist_function("dispatch_suspend")
            .allowlist_function("dispatch_source_cancel")
            .allowlist_function("dispatch_set_context")
            .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
            .layout_tests(false)
            .generate()
            .expect("unable to generate bindings");

        let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
        bindings
            .write_to_file(out_path.join("dispatch_sys.rs"))
            .expect("couldn't write dispatch bindings");
    }

    fn generate_shader_bindings() -> PathBuf {
        let output_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("scene.h");
        let crate_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
        let mut config = Config {
            include_guard: Some("SCENE_H".into()),
            language: cbindgen::Language::C,
            no_includes: true,
            ..Default::default()
        };
        config.export.include.extend([
            "Bounds".into(),
            "Corners".into(),
            "Edges".into(),
            "Size".into(),
            "Pixels".into(),
            "PointF".into(),
            "Hsla".into(),
            "ContentMask".into(),
            "Uniforms".into(),
            "AtlasTile".into(),
            "PathRasterizationInputIndex".into(),
            "PathVertex_ScaledPixels".into(),
            "ShadowInputIndex".into(),
            "Shadow".into(),
            "QuadInputIndex".into(),
            "Underline".into(),
            "UnderlineInputIndex".into(),
            "Quad".into(),
            "BorderStyle".into(),
            "SpriteInputIndex".into(),
            "MonochromeSprite".into(),
            "PolychromeSprite".into(),
            "PathSprite".into(),
            "SurfaceInputIndex".into(),
            "SurfaceBounds".into(),
            "TransformationMatrix".into(),
        ]);
        config.no_includes = true;
        config.enumeration.prefix_with_name = true;

        let mut builder = cbindgen::Builder::new();

        let src_paths = [
            crate_dir.join("src/scene.rs"),
            crate_dir.join("src/geometry.rs"),
            crate_dir.join("src/color.rs"),
            crate_dir.join("src/window.rs"),
            crate_dir.join("src/platform.rs"),
            crate_dir.join("src/platform/mac/metal_renderer.rs"),
        ];
        for src_path in src_paths {
            println!("cargo:rerun-if-changed={}", src_path.display());
            builder = builder.with_src(src_path);
        }

        builder
            .with_config(config)
            .generate()
            .expect("Unable to generate bindings")
            .write_to_file(&output_path);

        output_path
    }

    fn compile_metal_shaders(header_path: &Path) {
        use std::process::{self, Command};
        let shader_path = "./src/platform/mac/shaders.metal";
        let air_output_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("shaders.air");
        let metallib_output_path =
            PathBuf::from(env::var("OUT_DIR").unwrap()).join("shaders.metallib");
        println!("cargo:rerun-if-changed={}", shader_path);

        let output = Command::new("xcrun")
            .args([
                "-sdk",
                "macosx",
                "metal",
                "-gline-tables-only",
                "-mmacosx-version-min=10.15.7",
                "-MO",
                "-c",
                shader_path,
                "-include",
                (header_path.to_str().unwrap()),
                "-o",
            ])
            .arg(&air_output_path)
            .output()
            .unwrap();

        if !output.status.success() {
            eprintln!(
                "metal shader compilation failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            process::exit(1);
        }

        let output = Command::new("xcrun")
            .args(["-sdk", "macosx", "metallib"])
            .arg(air_output_path)
            .arg("-o")
            .arg(metallib_output_path)
            .output()
            .unwrap();

        if !output.status.success() {
            eprintln!(
                "metallib compilation failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            process::exit(1);
        }
    }
}
