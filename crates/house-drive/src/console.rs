//! Questions asked at the console, for `join` and `set-credentials`.

use std::io::{self, Write};

use windows::Win32::System::Console::{
    CONSOLE_MODE, ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
};

pub fn ask(question: &str) -> Result<String, String> {
    print!("{question}: ");
    io::stdout().flush().map_err(|error| error.to_string())?;
    let mut line = String::new();
    io::stdin().read_line(&mut line).map_err(|error| error.to_string())?;
    return Ok(line.trim().to_string());
}

pub fn ask_yes_no(question: &str) -> Result<bool, String> {
    loop {
        match ask(&format!("{question} (y/n)"))?.to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => {}
        }
    }
}

/// Asks without echoing what is typed. Needs a real console: mintty (Git Bash's own window)
/// gives programs a pipe instead, so there the question is refused rather than echoed.
pub fn ask_hidden(question: &str) -> Result<String, String> {
    // SAFETY: console calls on this process's standard input handle; the mode is restored below.
    let (handle, mode) = unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE).map_err(|error| error.to_string())?;
        let mut mode = CONSOLE_MODE(0);
        if GetConsoleMode(handle, &mut mode).is_err() {
            return Err("no console to type a password into without showing it: run this from Windows \
                        Terminal or another real console, not mintty"
                .to_string());
        }
        SetConsoleMode(handle, CONSOLE_MODE(mode.0 & !ENABLE_ECHO_INPUT.0)).map_err(|error| error.to_string())?;
        (handle, mode)
    };
    let answer = ask(question);
    // SAFETY: as above.
    unsafe {
        let _ = SetConsoleMode(handle, mode);
    }
    println!();
    return answer;
}
