//! Windowless build of strobe for the autostarted daemon (the pythonw / javaw convention).
#![windows_subsystem = "windows"]

fn main() {
    std::process::exit(strobe::app::main());
}
