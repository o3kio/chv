fn main() {
    let proto_dir =
        std::path::PathBuf::from(std::env!("CARGO_MANIFEST_DIR")).join("../../../proto");
    let protos = [
        proto_dir.join("node/chv-stord-api.proto"),
        proto_dir.join("node/chv-stord-migration.proto"),
    ];

    // `tonic_prost_build` does not emit rerun directives itself, so without
    // these a cached build script keeps serving code generated from an older
    // protocol after a `.proto` edit. That is especially easy to hit in CI,
    // where `target/` is restored from a cache keyed on `Cargo.lock`.
    for proto in &protos {
        println!("cargo:rerun-if-changed={}", proto.display());
    }

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&protos, std::slice::from_ref(&proto_dir))
        .expect("Failed to compile protos");
}
