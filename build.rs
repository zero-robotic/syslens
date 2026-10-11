fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = [
        "proto/common.proto",
        "proto/system.proto",
        "proto/process.proto",
    ];
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    let mut config = prost_build::Config::new();
    config.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    config.file_descriptor_set_path(out.join("syslens.bin"));
    config.compile_protos(
        &protos,
        &[
            std::path::PathBuf::from("proto"),
            protoc_bin_vendored::include_path()?,
        ],
    )?;
    for path in protos {
        println!("cargo:rerun-if-changed={path}");
    }
    Ok(())
}
