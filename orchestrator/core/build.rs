// Build script for aegis-core
// Compiles Protocol Buffer definitions for gRPC

fn main() -> Result<(), Box<dyn std::error::Error>> {
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
