fn main() -> anyhow::Result<()> {
    // Rebuild when C component sources change
    println!("cargo:rerun-if-changed=components/ssh_bridge/ssh_bridge.c");
    println!("cargo:rerun-if-changed=components/ssh_bridge/include/ssh_bridge.h");
    println!("cargo:rerun-if-changed=components/ssh_bridge/CMakeLists.txt");

    embuild::build::CfgArgs::output_propagated("ESP_IDF")?;
    embuild::build::LinkArgs::output_propagated("ESP_IDF")
}
