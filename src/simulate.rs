use std::process::Command;

pub fn run_simc(input_file: &str, output_json: &str) -> std::io::Result<()> {
    let status = Command::new(r"..\simc\simc.exe")
        .arg(input_file)
        .arg(format!("json2={}", output_json))
        .status()?;

    if !status.success() {
        println!("SimulationCraft error: {:?}", status);
    }
    Ok(())
}
