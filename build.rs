fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(
            &["proto/etcdserverpb/rpc.proto"],
            &["proto"],
        )?;

    // Also compile the mvccpb and authpb protos standalone so we can
    // reference them directly if needed.
    tonic_build::configure()
        .build_server(false)
        .build_client(false)
        .compile_protos(
            &[
                "proto/mvccpb/kv.proto",
                "proto/authpb/auth.proto",
            ],
            &["proto"],
        )?;

    Ok(())
}
