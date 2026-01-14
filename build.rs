fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "examples")]
    tonic_prost_build::compile_protos("examples/proto/greeter.proto")?;
    Ok(())
}
