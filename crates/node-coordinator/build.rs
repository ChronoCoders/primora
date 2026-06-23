fn main() {
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(
            &["../../proto/node_service.proto"],
            &["../../proto"],
        )
        .unwrap();
}
