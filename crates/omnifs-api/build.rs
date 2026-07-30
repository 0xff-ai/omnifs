fn main() {
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("locate vendored protoc");
    println!("cargo:rerun-if-changed=proto/control/v1/control.proto");
    let mut config = prost_build::Config::new();
    config.protoc_executable(protoc);
    tonic_prost_build::configure()
        .bytes(".omnifs.control.v1")
        .compile_with_config(config, &["proto/control/v1/control.proto"], &["proto"])
        .expect("compile control protobuf");
}
