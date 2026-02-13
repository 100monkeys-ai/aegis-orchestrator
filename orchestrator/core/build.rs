// Build script for aegis-core
// Compiles Protocol Buffer definitions for gRPC

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Set PROTOC environment variable to point to the vendored protoc binary
    std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path().unwrap());

    // Compile aegis_runtime.proto
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile(
            &["../../proto/aegis_runtime.proto"],
            &["../../proto"],
        )?;

    println!("cargo:rerun-if-changed=../../proto/aegis_runtime.proto");

    Ok(())
}
