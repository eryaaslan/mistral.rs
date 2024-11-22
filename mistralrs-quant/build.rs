fn main() {
    #[cfg(feature = "cuda")]
    {
        use std::{fs::read_to_string, path::PathBuf, process::Command, vec};
        const MARLIN_FFI_PATH: &str = "src/gptq/marlin_ffi.rs";
        const CUDA_NVCC_FLAGS: Option<&'static str> = option_env!("CUDA_NVCC_FLAGS");

        println!("cargo:rerun-if-changed=build.rs");

        let compute_cap = {
            let mut cmd = Command::new("nvidia-smi");
            let output = String::from_utf8(
                cmd.args(["--query-gpu=compute_cap", "--format=csv"])
                    .output()
                    .expect("Failed to get compute cap")
                    .stdout,
            )
            .expect("Output of nvidia-smi was not utf8.");
            (output
                .split('\n')
                .nth(1)
                .unwrap()
                .trim()
                .parse::<f32>()
                .unwrap()
                * 100.) as usize
        };

        // ======== Handle optional marlin kernel compilation
        let compile_marlin = compute_cap >= 800;
        let mut marlin_ffi_ct = read_to_string(MARLIN_FFI_PATH).unwrap();
        let have_marlin = match compile_marlin {
            true => "true",
            false => "false",
        };
        if marlin_ffi_ct.contains("pub(crate) const HAVE_MARLIN_KERNELS: bool = true;") {
            marlin_ffi_ct = marlin_ffi_ct.replace(
                "pub(crate) const HAVE_MARLIN_KERNELS: bool = true;",
                &format!("pub(crate) const HAVE_MARLIN_KERNELS: bool = {have_marlin};"),
            );
        } else {
            marlin_ffi_ct = marlin_ffi_ct.replace(
                "pub(crate) const HAVE_MARLIN_KERNELS: bool = false;",
                &format!("pub(crate) const HAVE_MARLIN_KERNELS: bool = {have_marlin};"),
            );
        }
        std::fs::write(MARLIN_FFI_PATH, marlin_ffi_ct).unwrap();
        // ========

        let build_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
        let mut lib_files = vec![
            "kernels/gptq/q_gemm.cu",
            "kernels/hqq/hqq.cu",
            "kernels/ops/ops.cu",
        ];
        if compile_marlin {
            lib_files.push("kernels/marlin/marlin_kernel.cu");
        } else {
            lib_files.push("kernels/marlin/dummy_marlin_kernel.cu");
        }
        for lib_file in lib_files.iter() {
            println!("cargo:rerun-if-changed={lib_file}");
        }
        let mut builder = bindgen_cuda::Builder::default()
            .kernel_paths(lib_files)
            .out_dir(build_dir.clone())
            .arg("-std=c++17")
            .arg("-O3")
            .arg("-U__CUDA_NO_HALF_OPERATORS__")
            .arg("-U__CUDA_NO_HALF_CONVERSIONS__")
            .arg("-U__CUDA_NO_HALF2_OPERATORS__")
            .arg("-U__CUDA_NO_BFLOAT16_CONVERSIONS__")
            .arg("--expt-relaxed-constexpr")
            .arg("--expt-extended-lambda")
            .arg("--use_fast_math")
            .arg("--verbose");

        // https://github.com/EricLBuehler/mistral.rs/issues/286
        if let Some(cuda_nvcc_flags_env) = CUDA_NVCC_FLAGS {
            builder = builder.arg("--compiler-options");
            builder = builder.arg(cuda_nvcc_flags_env);
        }

        let target = std::env::var("TARGET").unwrap();
        let build_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
        // https://github.com/EricLBuehler/mistral.rs/issues/588
        let out_file = if target.contains("msvc") {
            // Windows case
            build_dir.join("mistralrsquant.lib")
        } else {
            build_dir.join("libmistralrsquant.a")
        };
        builder.build_lib(out_file);
        println!("cargo:rustc-link-search={}", build_dir.display());
        println!("cargo:rustc-link-lib=mistralrsquant");
        println!("cargo:rustc-link-lib=dylib=cudart");

        if target.contains("msvc") {
            // nothing to link to
        } else if target.contains("apple")
            || target.contains("freebsd")
            || target.contains("openbsd")
        {
            println!("cargo:rustc-link-lib=dylib=c++");
        } else if target.contains("android") {
            println!("cargo:rustc-link-lib=dylib=c++_shared");
        } else {
            println!("cargo:rustc-link-lib=dylib=stdc++");
        }
    }
}