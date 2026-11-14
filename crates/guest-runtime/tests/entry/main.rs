mod cli;
mod relro;

pub type Scenario = (&'static str, fn(&str));

fn main() {
    let arguments: Vec<_> = std::env::args().collect();
    let Some(binary) = arguments
        .windows(2)
        .find(|args| args[0] == "--binary")
        .map(|args| args[1].as_str())
    else {
        println!("guest entry: run --test entry -- --binary PATH");
        return;
    };
    let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(binary)
        .canonicalize()
        .unwrap();
    let mut passed = 0;
    for &(name, scenario) in relro::SCENARIOS.iter().chain(cli::SCENARIOS) {
        println!("guest entry: {name}");
        scenario(binary.to_str().unwrap());
        passed += 1;
    }
    println!("guest entry: {passed} scenarios passed");
}
