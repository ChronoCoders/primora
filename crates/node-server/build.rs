fn main() {
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .compile_protos(
            &["../../proto/node_service.proto"],
            &["../../proto"],
        )
        .unwrap();
}
